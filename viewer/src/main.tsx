import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
// Fonts are bundled so the app makes no third-party requests.
import "@fontsource/inter/400.css";
import "@fontsource/inter/500.css";
import "@fontsource/inter/600.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "@fontsource/jetbrains-mono/600.css";
import "@fontsource/space-grotesk/500.css";
import "@fontsource/space-grotesk/600.css";
import "@fontsource/space-grotesk/700.css";
import App from "./App";
import MonitorWindow from "./screens/MonitorWindow";
import FileTransferWindow from "./screens/FileTransferWindow";

// Secondary windows open with a query flag; the main app runs in every other
// window. ?monitor=<id>&label=<name> for a monitor, ?files=1 for file transfer.
const params = new URLSearchParams(window.location.search);
const monitorParam = params.get("monitor");
const monitor = monitorParam === null ? null : Number(monitorParam);

function Root() {
  if (params.get("files") === "1") return <FileTransferWindow />;
  if (monitor !== null && Number.isFinite(monitor)) return <MonitorWindow monitor={monitor} label={params.get("label") ?? `Monitor ${monitor}`} />;
  return <App />;
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <Root />
  </StrictMode>,
);
