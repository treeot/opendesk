import { type FormEvent, useEffect, useState } from "react";
import { type AccessRules as Rules, type AllowMode, type HoursRule, getAccessRules, getIdleTimeout, setAccessRules, setIdleTimeout } from "../lib/api";

const DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MODES: { mode: AllowMode; label: string; detail: string }[] = [
  { mode: "mark_known", label: "Ask for everyone", detail: "Listed devices are marked as known in the approval prompt." },
  { mode: "auto_accept", label: "Accept listed devices, ask for others", detail: "Listed devices connect without the prompt. They still need the password." },
  { mode: "only_listed", label: "Only allow listed devices", detail: "Other IDs are turned away without a prompt, even with unattended access." },
];
export const IDLE_CHOICES = [0, 5, 15, 30, 60];

/** Minutes after midnight as "HH:MM". */
export const toClock = (minutes: number) => `${Math.floor(minutes / 60).toString().padStart(2, "0")}:${(minutes % 60).toString().padStart(2, "0")}`;
/** "HH:MM" as minutes after midnight, or null when it isn't a time. */
export const fromClock = (value: string): number | null => {
  const match = /^(\d{1,2}):(\d{2})$/.exec(value);
  if (!match) return null;
  const hours = Number(match[1]);
  const minutes = Number(match[2]);
  return hours < 24 && minutes < 60 ? hours * 60 + minutes : null;
};

/** One line describing when a window applies, for screen readers and the summary. */
export const describeRule = (rule: HoursRule) => {
  const days = [...rule.days].sort((a, b) => a - b).map((day) => DAYS[day]).join(", ") || "No days";
  if (rule.start === rule.end) return `${days}, all day`;
  const overnight = rule.end < rule.start ? " (next day)" : "";
  return `${days}, ${toClock(rule.start)} to ${toClock(rule.end)}${overnight}`;
};

const errorText = (error: unknown) => (typeof error === "string" ? error : error instanceof Error ? error.message : "Could not save the access rules.");

/** Allowed viewer IDs, how they are used, and allowed connection hours. */
export default function AccessRules() {
  const [saved, setSaved] = useState<Rules | null>(null);
  const [draft, setDraft] = useState<Rules | null>(null);
  const [newId, setNewId] = useState("");
  const [newLabel, setNewLabel] = useState("");
  const [message, setMessage] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    let mounted = true;
    void getAccessRules().then((rules) => { if (mounted) { setSaved(rules); setDraft(rules); } }).catch((error) => { if (mounted) setMessage(errorText(error)); });
    return () => { mounted = false; };
  }, []);

  if (!draft) return <section className="card access-rules-card"><span className="eyebrow">ACCESS RULES</span><h2>Who can connect</h2>{message ? <p className="message">{message}</p> : <p className="caption">Loading...</p>}</section>;

  const dirty = JSON.stringify(draft) !== JSON.stringify(saved);
  const update = (change: Partial<Rules>) => { setDraft({ ...draft, ...change }); setMessage(null); };
  const updateRule = (index: number, change: Partial<HoursRule>) => update({ hours: { ...draft.hours, rules: draft.hours.rules.map((rule, at) => at === index ? { ...rule, ...change } : rule) } });

  const addId = (event: FormEvent) => {
    event.preventDefault();
    const id = newId.replace(/\D/g, "");
    if (id.length !== 9) { setMessage("Enter the 9-digit ID."); return; }
    if (draft.allowed.some((entry) => entry.id === id)) { setMessage("That ID is already on the list."); return; }
    update({ allowed: [...draft.allowed, { id, label: newLabel.trim() }] });
    setNewId("");
    setNewLabel("");
  };

  const save = async () => {
    if (draft.mode === "only_listed" && draft.allowed.length === 0) { setMessage("Add at least one ID before allowing only listed devices."); return; }
    if (draft.hours.enabled && draft.hours.rules.length === 0) { setMessage("Add at least one time range, or turn allowed hours off."); return; }
    if (draft.hours.rules.some((rule) => rule.days.length === 0)) { setMessage("Pick at least one day for each time range."); return; }
    setSaving(true);
    try {
      const stored = await setAccessRules(draft);
      setSaved(stored);
      setDraft(stored);
      setMessage("Saved. The rules apply from the next connection.");
    } catch (error) {
      setMessage(errorText(error));
    } finally {
      setSaving(false);
    }
  };

  return (
    <section className="card access-rules-card">
      <span className="eyebrow">ACCESS RULES</span>
      <h2>Who can connect</h2>
      <p className="caption">IDs come from the relay and can be faked, so every viewer still needs the password. Use the list to narrow who may try, not as a password.</p>
      <fieldset className="access-modes">
        <legend className="access-rules__legend">Listed devices</legend>
        {MODES.map((option) => (
          <label key={option.mode} className="access-mode">
            <input type="radio" name="allow-mode" checked={draft.mode === option.mode} onChange={() => update({ mode: option.mode })} />
            <span><strong>{option.label}</strong><small>{option.detail}</small></span>
          </label>
        ))}
      </fieldset>
      {draft.allowed.length > 0
        ? <div className="contact-list">{draft.allowed.map((entry) => (
          <div className="contact-row access-id-row" key={entry.id}>
            <span className="mono">{entry.id}</span>
            <input className="access-label-input" value={entry.label} maxLength={64} placeholder="Label (optional)" aria-label={`Label for ${entry.id}`} onChange={(event) => update({ allowed: draft.allowed.map((item) => item.id === entry.id ? { ...item, label: event.target.value } : item) })} />
            <button className="icon-button" type="button" onClick={() => update({ allowed: draft.allowed.filter((item) => item.id !== entry.id) })} aria-label={`Remove allowed device ${entry.id}`}>×</button>
          </div>
        ))}</div>
        : <p className="caption">No devices are listed yet.</p>}
      <form className="contact-form" onSubmit={addId}>
        <input className="mono" inputMode="numeric" maxLength={11} value={newId} onChange={(event) => setNewId(event.target.value.replace(/[^\d ]/g, ""))} placeholder="Device ID" aria-label="Allowed device ID" />
        <input value={newLabel} maxLength={64} onChange={(event) => setNewLabel(event.target.value)} placeholder="Label (optional)" aria-label="Allowed device label" />
        <button className="primary-button" type="submit">Add</button>
      </form>

      <div className="access-hours">
        <label className="toggle-label"><input type="checkbox" checked={draft.hours.enabled} onChange={(event) => update({ hours: { enabled: event.target.checked, rules: event.target.checked && draft.hours.rules.length === 0 ? [{ days: [0, 1, 2, 3, 4], start: 9 * 60, end: 17 * 60 }] : draft.hours.rules } })} /><span>Only accept connections during set hours</span></label>
        <p className="caption">In this computer's local time. Outside these hours every connection is turned away, unattended access included. An end time before the start runs past midnight.</p>
        {draft.hours.enabled && <div className="access-hours__rules">
          {draft.hours.rules.map((rule, index) => (
            <div className="access-hours__rule" key={index} aria-label={describeRule(rule)} role="group">
              <div className="access-days">{DAYS.map((day, dayIndex) => {
                const on = rule.days.includes(dayIndex);
                return <button key={day} type="button" className={`access-day${on ? " access-day--on" : ""}`} aria-pressed={on} onClick={() => updateRule(index, { days: on ? rule.days.filter((d) => d !== dayIndex) : [...rule.days, dayIndex].sort((a, b) => a - b) })}>{day}</button>;
              })}</div>
              <input type="time" value={toClock(rule.start)} aria-label="From" onChange={(event) => { const value = fromClock(event.target.value); if (value !== null) updateRule(index, { start: value }); }} />
              <span className="caption access-hours__to">to</span>
              <input type="time" value={toClock(rule.end)} aria-label="Until" onChange={(event) => { const value = fromClock(event.target.value); if (value !== null) updateRule(index, { end: value }); }} />
              <button className="icon-button" type="button" onClick={() => update({ hours: { ...draft.hours, rules: draft.hours.rules.filter((_, at) => at !== index) } })} aria-label={`Remove ${describeRule(rule)}`}>×</button>
            </div>
          ))}
          <button className="quiet-button" type="button" onClick={() => update({ hours: { ...draft.hours, rules: [...draft.hours.rules, { days: [5, 6], start: 0, end: 0 }] } })}>Add time range</button>
        </div>}
      </div>

      <div className="access-rules__footer">
        {message && <p className={message.startsWith("Saved") ? "caption" : "message"} role="status">{message}</p>}
        <button className="primary-button" type="button" disabled={!dirty || saving} onClick={() => void save()}>{saving ? "Saving..." : "Save rules"}</button>
      </div>
    </section>
  );
}

/** How long a hosted session may go unused before the host ends it. */
export function IdleTimeout() {
  const [minutes, setMinutes] = useState<number | null>(null);
  const [message, setMessage] = useState<string | null>(null);

  useEffect(() => {
    let mounted = true;
    void getIdleTimeout().then((value) => { if (mounted) setMinutes(value); }).catch(() => { if (mounted) setMinutes(0); });
    return () => { mounted = false; };
  }, []);

  const change = async (value: number) => {
    const before = minutes;
    setMinutes(value);
    try { await setIdleTimeout(value); setMessage(null); } catch (error) { setMinutes(before); setMessage(errorText(error)); }
  };

  const choices = minutes !== null && !IDLE_CHOICES.includes(minutes) ? [...IDLE_CHOICES, minutes].sort((a, b) => a - b) : IDLE_CHOICES;
  return (
    <section className="card idle-timeout-card">
      <span className="eyebrow">IDLE SESSIONS</span>
      <h2>Disconnect unused sessions</h2>
      <p className="caption">Counts mouse and keyboard input, files, clipboard and chat from the viewer. The viewer is warned a minute before and can stay connected.</p>
      <label className="recordings-cap__select">Disconnect after&nbsp;
        <select value={minutes ?? 0} disabled={minutes === null} onChange={(event) => void change(Number(event.target.value))} aria-label="Disconnect after minutes without input">
          {choices.map((value) => <option key={value} value={value}>{value === 0 ? "Never" : `${value} minutes without input`}</option>)}
        </select>
      </label>
      {message && <p className="message">{message}</p>}
    </section>
  );
}
