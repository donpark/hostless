const DEFAULT_STORAGE_KEYS = {
  state: "hostless_state",
  token: "hostless_token",
  proxyBase: "hostless_proxy_base",
};

function stripUndefined(value) {
  return Object.fromEntries(
    Object.entries(value).filter(([, entry]) => entry !== undefined && entry !== null),
  );
}

function readQueryParams(search) {
  return new URLSearchParams(search || "");
}

function readHashParams(hash) {
  return new URLSearchParams((hash || "").replace(/^#/, ""));
}

export function shortToken(token) {
  if (!token) return "none";
  return `${token.slice(0, 14)}...${token.slice(-4)}`;
}

export function isLocalhostSubdomain(locationObject = window.location) {
  const hostname = locationObject.hostname || "";
  return hostname.endsWith(".localhost") && hostname !== "localhost";
}

export function inferHostlessMode(locationObject = window.location, connected = false) {
  if (isLocalhostSubdomain(locationObject)) {
    return "local-direct";
  }
  if (connected) {
    return "remote-connected";
  }
  return "remote-handshake";
}

export function buildLocalApiBase(locationObject = window.location) {
  const protocol = locationObject.protocol === "https:" ? "https:" : "http:";
  return `${protocol}//localhost:${locationObject.port}`;
}

export function readHostlessCallback(locationObject = window.location) {
  const query = readQueryParams(locationObject.search);
  const hash = readHashParams(locationObject.hash);

  const token = hash.get("token");
  const localUrl = query.get("local_url");
  const port = query.get("port");
  const state = query.get("state");
  const expiresIn = query.get("expires_in");

  if (!token || (!localUrl && !port)) {
    return null;
  }

  return {
    token,
    proxyBase: localUrl || `http://localhost:${port}`,
    localUrl,
    port,
    state,
    expiresIn: expiresIn ? Number(expiresIn) : null,
  };
}

export function buildSchemeRegistrationUrl(payload, schemeBase = "authmatic://register") {
  const encoded = new URLSearchParams();
  encoded.set("data", JSON.stringify(payload));
  encoded.set("origin", payload.origin);
  if (payload.callback) encoded.set("callback", payload.callback);
  if (payload.state) encoded.set("state", payload.state);
  if (payload.allowed_providers) {
    encoded.set("allowed_providers", payload.allowed_providers.join(","));
  }
  if (payload.allowed_models) {
    encoded.set("allowed_models", payload.allowed_models.join(","));
  }
  if (typeof payload.rate_limit === "number") {
    encoded.set("rate_limit", String(payload.rate_limit));
  }
  return `${schemeBase}?${encoded.toString()}`;
}

function readStoredConnection(storage, storageKeys) {
  return {
    token: storage.getItem(storageKeys.token) || "",
    proxyBase: storage.getItem(storageKeys.proxyBase) || "",
  };
}

function persistConnection(storage, storageKeys, connection) {
  storage.setItem(storageKeys.token, connection.token);
  storage.setItem(storageKeys.proxyBase, connection.proxyBase);
}

function clearStoredConnection(storage, storageKeys) {
  storage.removeItem(storageKeys.token);
  storage.removeItem(storageKeys.proxyBase);
}

function clearCallbackUrl(locationObject, historyObject) {
  const url = new URL(locationObject.href);
  const query = readQueryParams(url.search);
  ["port", "local_url", "state", "expires_in"].forEach((key) => query.delete(key));

  const hash = readHashParams(url.hash);
  hash.delete("token");

  const nextSearch = query.toString();
  const nextHash = hash.toString();
  const nextUrl = `${url.pathname}${nextSearch ? `?${nextSearch}` : ""}${nextHash ? `#${nextHash}` : ""}`;
  historyObject.replaceState({}, document.title, nextUrl);
}

function normalizeRegistrationResponse(data, fallbackProxyBase) {
  return {
    token: data.token,
    proxyBase: data.local_url || fallbackProxyBase || `http://localhost:${data.port}`,
    expiresIn: typeof data.expires_in === "number" ? data.expires_in : null,
  };
}

async function parseJsonResponse(response) {
  const text = await response.text();
  let data = null;

  if (text) {
    try {
      data = JSON.parse(text);
    } catch (_error) {
      data = null;
    }
  }

  if (!response.ok) {
    const message =
      (data && data.error && (data.error.message || data.error)) ||
      text ||
      `Request failed with status ${response.status}`;
    throw new Error(String(message));
  }

  return data || {};
}

export function createHostlessClient(options = {}) {
  const locationObject = options.location || window.location;
  const historyObject = options.history || window.history;
  const fetchImpl = options.fetch || window.fetch.bind(window);
  const storage = options.storage || window.localStorage;
  const cryptoObject = options.crypto || window.crypto;
  const launcher = options.launcher || ((url) => {
    locationObject.href = url;
  });
  const storageKeys = { ...DEFAULT_STORAGE_KEYS, ...(options.storageKeys || {}) };

  let connection = readStoredConnection(storage, storageKeys);

  function getConnection() {
    return { ...connection };
  }

  function isConnected() {
    return Boolean(connection.token && connection.proxyBase);
  }

  function getMode() {
    return inferHostlessMode(locationObject, isConnected());
  }

  function disconnect() {
    clearStoredConnection(storage, storageKeys);
    storage.removeItem(storageKeys.state);
    connection = { token: "", proxyBase: "" };
    return getConnection();
  }

  function storeConnection(nextConnection) {
    connection = {
      token: nextConnection.token,
      proxyBase: nextConnection.proxyBase,
      expiresIn: nextConnection.expiresIn || null,
    };
    persistConnection(storage, storageKeys, connection);
    return getConnection();
  }

  function initialize() {
    const callbackData = readHostlessCallback(locationObject);
    let error = null;

    if (callbackData) {
      const expectedState = storage.getItem(storageKeys.state);
      if (expectedState && callbackData.state && expectedState !== callbackData.state) {
        error = new Error("State mismatch detected. Please reconnect.");
      } else {
        storeConnection(callbackData);
        storage.removeItem(storageKeys.state);
      }
      clearCallbackUrl(locationObject, historyObject);
    } else {
      connection = readStoredConnection(storage, storageKeys);
    }

    return {
      connection: getConnection(),
      callbackData,
      error,
      mode: getMode(),
    };
  }

  async function connect(connectOptions = {}) {
    if (isLocalhostSubdomain(locationObject)) {
      const apiBase = buildLocalApiBase(locationObject);
      const payload = stripUndefined({
        origin: locationObject.origin,
        allowed_providers: connectOptions.allowedProviders,
        allowed_models: connectOptions.allowedModels,
        rate_limit: connectOptions.rateLimit,
      });

      const response = await fetchImpl(`${apiBase}/auth/register`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
        },
        body: JSON.stringify(payload),
      });

      const data = await parseJsonResponse(response);
      const nextConnection = normalizeRegistrationResponse(data, apiBase);

      return {
        launched: false,
        mode: getMode(),
        connection: storeConnection(nextConnection),
        source: "local-direct",
      };
    }

    const callback = connectOptions.callback || `${locationObject.origin}${locationObject.pathname}`;
    const state = connectOptions.state || cryptoObject.randomUUID();
    const payload = stripUndefined({
      origin: locationObject.origin,
      callback,
      state,
      allowed_providers: connectOptions.allowedProviders,
      allowed_models: connectOptions.allowedModels,
      rate_limit: connectOptions.rateLimit,
    });

    storage.setItem(storageKeys.state, state);

    const launchUrl = buildSchemeRegistrationUrl(payload, connectOptions.schemeBase);
    launcher(launchUrl);

    return {
      launched: true,
      mode: getMode(),
      url: launchUrl,
      source: "remote-handshake",
    };
  }

  async function fetchWithBridge(path, init = {}) {
    if (!isConnected()) {
      throw new Error("Connect to Hostless first to get a bridge token.");
    }

    const target = path.startsWith("http://") || path.startsWith("https://")
      ? path
      : `${connection.proxyBase.replace(/\/$/, "")}/${path.replace(/^\/+/, "")}`;

    const headers = new Headers(init.headers || {});
    headers.set("Authorization", `Bearer ${connection.token}`);

    const response = await fetchImpl(target, {
      ...init,
      headers,
    });

    const responseText = await response.clone().text();
    if (
      response.status === 401 &&
      (responseText.includes("Invalid or unknown bridge token") ||
        responseText.includes("Bridge token has expired"))
    ) {
      disconnect();
      throw new Error(
        "Your bridge token is no longer valid. Reconnect with Hostless to mint a new token.",
      );
    }

    return response;
  }

  return {
    connect,
    disconnect,
    fetchWithBridge,
    getConnection,
    getMode,
    initialize,
    isConnected,
    storageKeys,
  };
}