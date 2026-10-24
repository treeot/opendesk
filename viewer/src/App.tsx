import { useCallback, useEffect, useRef, useState } from "react";
import Home from "./screens/Home";
import Connecting from "./screens/Connecting";
import IncomingRequest from "./screens/IncomingRequest";
import IncomingFiles from "./components/IncomingFiles";
import Permissions from "./screens/Permissions";
import Session from "./screens/Session";
import TwofaPrompt from "./screens/TwofaPrompt";
import Toast from "./components/Toast";
import UpdateBanner from "./components/UpdateBanner";
import * as api from "./lib/api";
import { NotifyContext, friendlyError, type Notice, type NoticeTone } from "./lib/notify";

type Screen = "home" | "connecting" | "session" | "permissions";

export default function App() {
  const [screen, setScreen] = useState<Screen>("home");
  const screenRef = useRef<Screen>("home");
  const [status, setStatus] = useState<api.Status>(api.isDevBrowser ? "online" : "offline");
  const [viewerStatus, setViewerStatus] = useState<api.Status>("connecting");
  const [request, setRequest] = useState<api.IncomingRequest | null>(null);
  const [session, setSession] = useState<api.SessionStarted | null>(null);
  const [target, setTarget] = useState("");
  const [twofa, setTwofa] = useState<"required" | "invalid" | null>(null);
  const lastConnect = useRef<{ id: string; password: string } | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const noticeId = useRef(0);
  const permissionsChecked = useRef(false);

  const go = useCallback((next: Screen) => { screenRef.current = next; setScreen(next); }, []);
  const notify = useCallback((text: string, tone: NoticeTone = "error") => {
    noticeId.current += 1;
    setNotice({ id: noticeId.current, text, tone });
  }, []);
  const dismiss = useCallback(() => setNotice(null), []);

  // connect() returns once the attempt is spawned; the session screen waits
  // for the viewer-role session_started event, i.e. the host accepted.
  const handleConnect = async (remoteId: string, password: string, authCode?: string) => {
    setTarget(remoteId);
    lastConnect.current = { id: remoteId, password };
    setTwofa(null);
    setNotice(null);
    go("connecting");
    try {
      await (authCode ? api.connect(remoteId, password, authCode) : api.connect(remoteId, password));
    } catch (error) {
      if (screenRef.current === "connecting") go("home");
      throw error;
    }
  };
  const handleCancel = () => {
    go("home");
    void api.disconnect().catch((error: unknown) => notify(friendlyError(error)));
  };
  const handleDisconnect = useCallback(() => {
    void api.disconnect().catch((error: unknown) => notify(friendlyError(error)));
  }, [notify]);

  useEffect(() => {
    let active = true;
    const unlisten = Promise.all([
      api.onStatusChanged(({ state }) => {
        if (!active) return;
        setStatus(state);
        if (state === "online") setNotice(null);
      }),
      api.onViewerStatus(({ state }) => { if (active) setViewerStatus(state); }),
      api.onIncomingRequest((payload) => {
        if (!active) return;
        setRequest(payload);
      }),
      api.onSessionStarted((payload) => {
        if (!active) return;
        if (payload.role === "viewer" && screenRef.current !== "connecting") return;
        setRequest(null);
        setTwofa(null);
        setSession(payload);
        go("session");
        setNotice(null);
      }),
      api.onSessionEnded(() => {
        if (!active) return;
        setRequest(null);
        setSession(null);
        go("home");
      }),
      api.onError(({ message }) => active && notify(friendlyError(message))),
      api.onTwofaPrompt(({ reason }) => { if (active) setTwofa(reason); }),
      api.onHostNotice(({ message }) => active && notify(message, "info")),
      api.onMenuAction((action) => {
        if (!active) return;
        if (action === "settings") go("home");
        // Session handles fullscreen itself (with fit-to-screen) while it is up.
        else if (action === "fullscreen" && screenRef.current !== "session") {
          if (document.fullscreenElement) void document.exitFullscreen();
          else void document.documentElement.requestFullscreen?.();
        }
      }),
    ]);

    return () => {
      active = false;
      void unlisten.then((fns) => fns.forEach((fn) => fn()));
    };
  }, [go, notify]);

  // Once per launch, show the macOS permission checklist when something is
  // missing. Other platforms always report both as granted.
  useEffect(() => {
    if (permissionsChecked.current) return;
    permissionsChecked.current = true;
    void api.permissionStatus().then((granted) => {
      if ((!granted.screen_recording || !granted.accessibility) && screenRef.current === "home") go("permissions");
    }).catch(() => {});
  }, [go]);

  let content;
  if (screen === "permissions") {
    content = <Permissions onContinue={() => go("home")} />;
  } else if (screen === "connecting") {
    content = <Connecting remoteId={target} onCancel={handleCancel} />;
  } else if (screen === "session" && session) {
    content = <Session remoteId={session.remote_id} role={session.role} status={session.role === "viewer" ? viewerStatus : status} onDisconnect={handleDisconnect} />;
  } else {
    content = <Home status={status} initialTargetId={target} onConnect={handleConnect} onOpenPermissions={() => go("permissions")} />;
  }

  return <NotifyContext.Provider value={notify}>
    {content}
    <Toast notice={notice} onDismiss={dismiss} />
    <UpdateBanner enabled={!import.meta.env.DEV} visible={screen === "home" && !request} />
    {session?.role === "host" && <IncomingFiles />}
    {request && <IncomingRequest peerId={request.peer_id} known={request.known} onResolved={() => setRequest(null)} />}
    {twofa && <TwofaPrompt reason={twofa} onSubmit={(code) => {
      const last = lastConnect.current;
      setTwofa(null);
      if (last) void handleConnect(last.id, last.password, code).catch((error: unknown) => notify(friendlyError(error)));
    }} onCancel={() => setTwofa(null)} />}
  </NotifyContext.Provider>;
}
