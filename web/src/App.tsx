import { useEffect, useMemo, useRef, useState } from "react";
import { Connection, type ConnectionStatus, type Socket } from "./lib/connection";
import { friendlyError, unsupportedReason } from "./lib/errors";
import { DEFAULT_RELAY, saveRelay, savedRelay, viewerId } from "./lib/settings";
import type { Core, FileSignal, MonitorInfo } from "./lib/types";
import ConnectForm, { type ConnectRequest } from "./screens/ConnectForm";
import Session, { type SessionControls } from "./screens/Session";
import TwofaPrompt from "./screens/TwofaPrompt";

export interface AppProps {
  loadCore(): Promise<Core>;
  openSocket?: (url: string) => Socket;
  unsupported?: string | null;
}

interface Active {
  remoteId: string;
  status: ConnectionStatus;
}

export default function App({ loadCore, openSocket, unsupported = unsupportedReason() }: AppProps) {
  const [ownId] = useState(viewerId);
  const [active, setActive] = useState<Active | null>(null);
  const [monitors, setMonitors] = useState<MonitorInfo[]>([]);
  const [message, setMessage] = useState<string>();
  const [lastTarget, setLastTarget] = useState("");
  const [busy, setBusy] = useState(false);
  const [twofa, setTwofa] = useState<"required" | "invalid" | null>(null);
  const lastRequest = useRef<ConnectRequest | null>(null);
  const connection = useRef<Connection | null>(null);
  const videoSink = useRef<((frame: Uint8Array) => void) | null>(null);
  const audioSink = useRef<((payload: Uint8Array, channels: number) => void) | null>(null);
  const clipboardSink = useRef<((content: { type: "text"; text: string } | { type: "image"; width: number; height: number; png: Uint8Array }) => void) | null>(null);
  const fileSink = useRef<((signal: FileSignal) => void) | null>(null);

  const controls = useMemo<SessionControls>(() => ({
    sendInput: (event) => connection.current?.sendInput(event),
    requestKeyframe: () => connection.current?.requestKeyframe(),
    selectMonitor: (id) => connection.current?.selectMonitor(id),
    setVideoSink: (sink) => { videoSink.current = sink; },
    setAudioSink: (sink) => { audioSink.current = sink; },
    sendClipboard: (text) => connection.current?.sendClipboard(text),
    sendClipboardImage: (width, height, png) => connection.current?.sendClipboardImage(width, height, png),
    setClipboardSink: (sink) => { clipboardSink.current = sink; },
    sendFile: (id, name, data) => connection.current?.sendFile(id, name, data),
    acceptFile: (id) => connection.current?.acceptFile(id),
    rejectFile: (id) => connection.current?.rejectFile(id),
    setFileSink: (sink) => { fileSink.current = sink; },
  }), []);

  // Leaving the page ends the session on the host right away.
  useEffect(() => {
    const leave = () => connection.current?.disconnect();
    window.addEventListener("pagehide", leave);
    return () => window.removeEventListener("pagehide", leave);
  }, []);

  const connect = async (request: ConnectRequest) => {
    setBusy(true);
    setMessage(undefined);
    setTwofa(null);
    lastRequest.current = request;
    let core: Core;
    try {
      core = await loadCore();
    } catch (error) {
      setBusy(false);
      setMessage(`Couldn't load the viewer: ${friendlyError(error)}`);
      return;
    }
    saveRelay(request.relay);
    setLastTarget(request.target);
    setMonitors([]);
    const conn = new Connection(
      { relay: request.relay, target: request.target, viewerId: ownId, password: request.password, authCode: request.authCode },
      {
        status: (status) => setActive({ remoteId: request.target, status }),
        video: (frame) => videoSink.current?.(frame),
        audio: (payload, channels) => audioSink.current?.(payload, channels),
        monitors: setMonitors,
        clipboard: (content) => clipboardSink.current?.(content),
        file: (signal) => fileSink.current?.(signal),
        ended: (error) => {
          if (connection.current === conn) connection.current = null;
          setActive(null);
          setBusy(false);
          setMessage(error === null ? undefined : friendlyError(error));
        },
        twofa: (reason) => setTwofa(reason),
      },
      { core, openSocket },
    );
    connection.current = conn;
    conn.start();
  };

  const prompt = twofa && (
    <TwofaPrompt
      reason={twofa}
      onSubmit={(code) => {
        const base = lastRequest.current;
        setTwofa(null);
        if (base) void connect({ ...base, authCode: code });
      }}
      onCancel={() => setTwofa(null)}
    />
  );

  if (active) {
    return <>
      <Session remoteId={active.remoteId} status={active.status} monitors={monitors} controls={controls} onDisconnect={() => connection.current?.disconnect()} />
      {prompt}
    </>;
  }
  return (
    <>
      <ConnectForm
        viewerId={ownId}
        relay={savedRelay()}
        defaultRelay={DEFAULT_RELAY}
        message={message}
        unsupported={unsupported}
        busy={busy}
        initialTarget={lastTarget}
        onConnect={(request) => void connect(request)}
      />
      {prompt}
    </>
  );
}
