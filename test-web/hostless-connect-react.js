import * as React from "react";

import { createHostlessClient, shortToken } from "./hostless-connect.js";

export function useHostlessConnect(options = {}) {
  const clientRef = React.useRef(null);
  const [connection, setConnection] = React.useState({ token: "", proxyBase: "" });
  const [mode, setMode] = React.useState("detecting");
  const [error, setError] = React.useState("");
  const [busy, setBusy] = React.useState(false);

  if (!clientRef.current && typeof window !== "undefined") {
    clientRef.current = createHostlessClient(options.clientOptions);
  }

  React.useEffect(() => {
    if (!clientRef.current) return;

    const result = clientRef.current.initialize();
    setConnection(result.connection);
    setMode(result.mode);
    if (result.error) {
      setError(result.error.message);
    }
  }, []);

  async function connect(connectOptions = {}) {
    if (!clientRef.current) return null;

    setBusy(true);
    setError("");
    try {
      const result = await clientRef.current.connect(connectOptions);
      setConnection(clientRef.current.getConnection());
      setMode(clientRef.current.getMode());
      if (result.connection && options.onConnected) {
        options.onConnected(result.connection);
      }
      return result;
    } catch (connectError) {
      const message = connectError instanceof Error ? connectError.message : String(connectError);
      setError(message);
      if (options.onError) {
        options.onError(connectError);
      }
      throw connectError;
    } finally {
      setBusy(false);
    }
  }

  function disconnect() {
    if (!clientRef.current) return;
    clientRef.current.disconnect();
    setConnection(clientRef.current.getConnection());
    setMode(clientRef.current.getMode());
    setError("");
    if (options.onDisconnected) {
      options.onDisconnected();
    }
  }

  async function fetchWithHostless(path, init) {
    if (!clientRef.current) {
      throw new Error("Hostless client is not available.");
    }

    try {
      return await clientRef.current.fetchWithBridge(path, init);
    } catch (fetchError) {
      const message = fetchError instanceof Error ? fetchError.message : String(fetchError);
      setError(message);
      setConnection(clientRef.current.getConnection());
      setMode(clientRef.current.getMode());
      throw fetchError;
    }
  }

  return {
    busy,
    connect,
    connected: Boolean(connection.token && connection.proxyBase),
    connection,
    disconnect,
    error,
    fetchWithHostless,
    mode,
    tokenPreview: shortToken(connection.token),
  };
}

export function ConnectToHostlessButton(props) {
  const hostless = useHostlessConnect(props);
  const connectLabel = hostless.connected
    ? props.connectedLabel || "Reconnect Hostless"
    : props.label || "Connect to Hostless";
  const statusText = hostless.connected
    ? `Connected: ${hostless.connection.proxyBase} (${hostless.tokenPreview})`
    : "Not connected to Hostless.";

  return React.createElement(
    "div",
    { className: props.className || "hostless-connect" },
    React.createElement(
      "button",
      {
        type: "button",
        disabled: hostless.busy,
        onClick: () => {
          hostless.connect(props.connectOptions).catch(() => {});
        },
      },
      hostless.busy ? props.busyLabel || "Connecting..." : connectLabel,
    ),
    props.showDisconnect && hostless.connected
      ? React.createElement(
          "button",
          {
            type: "button",
            onClick: hostless.disconnect,
          },
          props.disconnectLabel || "Forget Hostless",
        )
      : null,
    React.createElement(
      "p",
      { className: props.statusClassName || "hostless-connect-status" },
      statusText,
    ),
    hostless.error
      ? React.createElement(
          "p",
          { className: props.errorClassName || "hostless-connect-error" },
          hostless.error,
        )
      : null,
  );
}