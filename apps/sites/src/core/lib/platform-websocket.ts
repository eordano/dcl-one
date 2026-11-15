// The module the bundler gives to a package that imports `ws`. The wallet packages import `ws` only
// as a fallback for a runtime with no WebSocket global; browsers and the Node versions this app
// supports have one.
const PlatformWebSocket = globalThis.WebSocket;

export { PlatformWebSocket as WebSocket };
export default PlatformWebSocket;
