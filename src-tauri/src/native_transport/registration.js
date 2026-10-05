(() => {
  if (window !== window.top || !['https://bdengine.app', 'https://beta.bdengine.app'].includes(location.origin)) return;
  const bridge = window.chrome?.webview;
  if (typeof bridge?.postMessageWithAdditionalObjects !== 'function') return;
  const send = bridge.postMessageWithAdditionalObjects.bind(bridge);
  // WebView2 rejects synthetic Files before delivering any native event. Route
  // only our registration failures through the same native error response path.
  Object.defineProperty(bridge, 'postMessageWithAdditionalObjects', {
    value(message, objects) {
      if (message?.type !== 'bde:file-register') return send(message, objects);
      try {
        return send(message, objects);
      } catch (_) {
        return bridge.postMessage(message);
      }
    }
  });
})();
