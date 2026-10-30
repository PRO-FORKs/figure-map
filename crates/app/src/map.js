// Google Maps JavaScript API 브리지. Dioxus webview에서 Rust 바인딩 없이 지도를 띄우는 유일한 경로라 JS로 둔다.
// Rust → JS: { key, elementId, fallback: { lat, lng } }
// JS → Rust: "loaded" | "located" | "location_denied" | "location_failed" | "load_failed" | "auth_failed"
const { key, elementId, fallback } = await dioxus.recv();

// 키가 거부되면 Maps API가 이 전역 콜백을 부른다.
window.gm_authFailure = () => dioxus.send("auth_failed");

if (!window.__sharedMapLoader) {
  window.__sharedMapLoader = new Promise((resolve, reject) => {
    window.__sharedMapReady = resolve;
    const script = document.createElement("script");
    script.src =
      "https://maps.googleapis.com/maps/api/js?key=" +
      encodeURIComponent(key) +
      "&loading=async&callback=__sharedMapReady";
    script.async = true;
    script.onerror = () => {
      window.__sharedMapLoader = null;
      script.remove();
      reject(new Error("maps script"));
    };
    document.head.appendChild(script);
  });
}

let Map, Circle;
try {
  await window.__sharedMapLoader;
  ({ Map, Circle } = await google.maps.importLibrary("maps"));
} catch {
  dioxus.send("load_failed");
  return;
}

const element = document.getElementById(elementId);
if (!element) {
  return;
}
const map = new Map(element, {
  center: fallback,
  zoom: 12,
  streetViewControl: false,
  mapTypeControl: false,
});
dioxus.send("loaded");

if (navigator.geolocation) {
  navigator.geolocation.getCurrentPosition(
    ({ coords }) => {
      const center = { lat: coords.latitude, lng: coords.longitude };
      map.setCenter(center);
      map.setZoom(16);
      new Circle({
        map,
        center,
        radius: Math.max(coords.accuracy, 10),
        strokeColor: "#1a73e8",
        strokeWeight: 1,
        fillColor: "#1a73e8",
        fillOpacity: 0.2,
      });
      dioxus.send("located");
    },
    (error) => {
      dioxus.send(error.code === error.PERMISSION_DENIED ? "location_denied" : "location_failed");
    },
    { enableHighAccuracy: true, timeout: 10000, maximumAge: 60000 },
  );
} else {
  dioxus.send("location_failed");
}

// 스크립트가 끝나면 Dioxus가 채널을 닫아 이후의 gm_authFailure를 받을 수 없다. 화면에서 지도가 사라질 때까지 열어 둔다.
await new Promise(() => {});
