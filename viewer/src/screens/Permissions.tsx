import { useCallback, useEffect, useState } from "react";
import * as api from "../lib/api";
import { friendlyError, useNotify } from "../lib/notify";

export interface PermissionsProps { onContinue: () => void }

export default function Permissions({ onContinue }: PermissionsProps) {
  const [permissions, setPermissions] = useState<api.PermissionStatus | null>(null);
  const notify = useNotify();

  const refresh = useCallback(() => {
    let active = true;
    void api.permissionStatus().then((status) => { if (active) setPermissions(status); });
    return () => { active = false; };
  }, []);

  useEffect(() => {
    const deactivate = refresh();
    const onFocus = () => { refresh(); };
    window.addEventListener("focus", onFocus);
    return () => { deactivate?.(); window.removeEventListener("focus", onFocus); };
  }, [refresh]);

  const screenGranted = permissions?.screen_recording === true;
  const accessibilityGranted = permissions?.accessibility === true;
  const grantedCount = Number(screenGranted) + Number(accessibilityGranted);

  const statusCard = (title: string, granted: boolean, which: "screen" | "accessibility") => (
    <section className="permission-card" key={which}>
      <div className="permission-card__info">
        <h2>{title}</h2>
        <p className={granted ? "permission-state granted" : "permission-state required"}><span className="permission-dot" aria-hidden="true" />{granted ? "Granted" : "Required"}</p>
      </div>
      {!granted && <button className="quiet-button" type="button" onClick={() => void api.openPermissionSettings(which).catch((error: unknown) => notify(friendlyError(error)))}>Open Settings</button>}
    </section>
  );

  return (
    <main className="permissions-screen">
      <div className="permissions-content">
        <button className="quiet-button permissions-back" type="button" onClick={onContinue}><span aria-hidden="true">←</span> Back</button>
        <span className="eyebrow permissions-kicker">SETUP</span>
        <h1>Permissions</h1>
        <p className="permissions-lead">Other devices can only see and control this computer once these are granted. You can still control other devices without them.</p>
        <div className="permission-cards">{statusCard("Screen Recording", screenGranted, "screen")}{statusCard("Accessibility", accessibilityGranted, "accessibility")}</div>
        {grantedCount < 2 && <p className="permissions-hint">Already enabled in System Settings? After an update macOS may ignore it: select OpenDesk, remove it with −, add it again with +, then restart OpenDesk.</p>}
        <div className="permissions-progress"><span>{grantedCount} of 2 granted</span><div className="progress-track"><div style={{ width: `${grantedCount * 50}%` }} /></div></div>
        {grantedCount === 2
          ? <button className="primary-button permissions-continue" type="button" onClick={onContinue}>Continue</button>
          : <button className="ghost-button permissions-continue" type="button" onClick={onContinue}>Skip for now</button>}
        <p className="permissions-footer">Permissions are enforced by macOS. OpenDesk cannot enable them for you.</p>
      </div>
    </main>
  );
}
