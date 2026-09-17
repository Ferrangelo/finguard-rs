// Routes the page's `/api/...` calls over Tauri's IPC.
//
// The page (frontend/src/services/api.ts) calls relative paths such as
// `/api/expenses` with plain `fetch` and knows nothing about this file, so a
// reader looking for the API transport in the page source will not find it.
// Android forces the indirection: its WebView gives a custom URI scheme no
// request body (wry 0.55.1 builds every custom scheme request with an empty
// body), so a protocol handler would lose every POST, PUT, and DELETE
// payload, and the app opens no network port because the backend has no
// authentication. IPC does carry a body, so each matching call goes to the
// `api_request` command in src/api_bridge.rs and comes back as a real
// `Response`.
//
// Tauri wraps this script in `(function () { ... })();`, so these
// declarations stay out of the page's global scope.

const API_PATH_PREFIX = "/api/";
const originalFetch = window.fetch.bind(window);

// Tauri's own init script defines `__TAURI_INTERNALS__.invoke` before this
// one runs, and the public `window.__TAURI__.core.invoke` is a wrapper around
// it. Reading it on each call rather than now keeps the shim working whatever
// order the init scripts run in.
function invokeApiRequest(payload) {
  const invoke = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;
  if (typeof invoke !== "function") {
    return Promise.reject(new Error("Tauri IPC is not available in this page"));
  }
  return invoke("api_request", payload);
}

// The same `{ "error": ... }` body every backend handler returns for a
// failure (backend/src/http_error.rs), so the page's error handling shows the
// message instead of reporting a bare rejected promise.
function apiErrorResponse(status, message) {
  return new Response(JSON.stringify({ error: message }), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function collectHeaders(source) {
  const headers = {};
  new Headers(source || undefined).forEach((value, name) => {
    headers[name] = value;
  });
  return headers;
}

window.fetch = async function (input, init) {
  const request = input instanceof Request ? input : null;
  const rawUrl = request ? request.url : String(input);

  let url;
  try {
    url = new URL(rawUrl, window.location.href);
  } catch {
    return originalFetch(input, init);
  }

  // Only this app's own API goes over IPC. Everything else keeps the
  // untouched `fetch`: the Google Fonts stylesheet, and on desktop Tauri's
  // own IPC requests to `http://ipc.localhost`, which would otherwise
  // recurse into this shim.
  if (url.origin !== window.location.origin || !url.pathname.startsWith(API_PATH_PREFIX)) {
    return originalFetch(input, init);
  }

  const options = init || {};
  const method = (options.method || (request && request.method) || "GET").toUpperCase();
  const headers = collectHeaders(options.headers || (request && request.headers));

  let body = null;
  if (options.body !== undefined && options.body !== null) {
    if (typeof options.body !== "string") {
      return apiErrorResponse(400, "This app can only send a text body to its own API.");
    }
    body = options.body;
  } else if (request) {
    body = (await request.text()) || null;
  }

  let result;
  try {
    result = await invokeApiRequest({
      method,
      path: url.pathname + url.search,
      headers,
      body,
    });
  } catch (error) {
    const detail = error && error.message ? error.message : String(error);
    return apiErrorResponse(500, `The app could not reach its own backend: ${detail}`);
  }

  // A 204, 205, or 304 response must carry a null body, or the `Response`
  // constructor throws.
  const carriesBody =
    result.status !== 204 && result.status !== 205 && result.status !== 304;
  return new Response(carriesBody ? result.body : null, {
    status: result.status,
    headers: result.contentType ? { "content-type": result.contentType } : {},
  });
};
