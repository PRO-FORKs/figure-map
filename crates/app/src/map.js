// Google Maps JavaScript API 브리지. Dioxus webview에서 Rust 바인딩 없이 지도를 띄우는 유일한 경로라 JS로 둔다.
// Rust → JS: 처음 { key, elementId, fallback: { lat, lng } }, 이후 명령
//   { type: "show_position", lat, lng, accuracy } | { type: "locate_in_browser" }
//   | { type: "show_listings", listings: [{ id, lat, lng, title }] } | { type: "clear_draft" }
// JS → Rust: { type: "loaded" | "located" | "location_denied" | "location_failed" | "load_failed" | "auth_failed" }
//   | { type: "clicked", lat, lng } | { type: "listing_clicked", id }
const { key, elementId, fallback } = await dioxus.recv();

// 키가 거부되면 Maps API가 이 전역 콜백을 부른다.
window.gm_authFailure = () => dioxus.send({ type: "auth_failed" });

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

let Map, Circle, Marker;
try {
  await window.__sharedMapLoader;
  ({ Map, Circle } = await google.maps.importLibrary("maps"));
  // AdvancedMarkerElement는 Map ID가 필요해 아직 설정이 없는 지금은 기본 Marker를 쓴다.
  ({ Marker } = await google.maps.importLibrary("marker"));
} catch {
  dioxus.send({ type: "load_failed" });
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
dioxus.send({ type: "loaded" });

let accuracyCircle = null;
let draftMarker = null;
const listingMarkers = new globalThis.Map();

map.addListener("click", (event) => {
  if (!event.latLng) {
    return;
  }
  const position = event.latLng.toJSON();
  if (draftMarker) {
    draftMarker.setPosition(position);
  } else {
    draftMarker = new Marker({ map, position, label: "+" });
  }
  dioxus.send({ type: "clicked", lat: position.lat, lng: position.lng });
});

function clearDraft() {
  if (draftMarker) {
    draftMarker.setMap(null);
    draftMarker = null;
  }
}

function showListings(listings) {
  const ids = new Set(listings.map((listing) => listing.id));
  for (const [id, marker] of listingMarkers) {
    if (!ids.has(id)) {
      marker.setMap(null);
      listingMarkers.delete(id);
    }
  }
  for (const { id, lat, lng, title } of listings) {
    const marker = listingMarkers.get(id);
    if (marker) {
      marker.setPosition({ lat, lng });
      marker.setTitle(title);
    } else {
      const created = new Marker({ map, position: { lat, lng }, title });
      created.addListener("click", () => dioxus.send({ type: "listing_clicked", id }));
      listingMarkers.set(id, created);
    }
  }
}

function showPosition(center, accuracy) {
  map.setCenter(center);
  map.setZoom(16);
  const radius = Math.max(accuracy, 10);
  if (accuracyCircle) {
    accuracyCircle.setCenter(center);
    accuracyCircle.setRadius(radius);
    return;
  }
  accuracyCircle = new Circle({
    map,
    center,
    radius,
    strokeColor: "#1a73e8",
    strokeWeight: 1,
    // 원 안을 눌러도 지도 클릭으로 매물을 추가할 수 있게 한다.
    clickable: false,
    fillColor: "#1a73e8",
    fillOpacity: 0.2,
  });
}

function locateInBrowser() {
  if (!navigator.geolocation) {
    dioxus.send({ type: "location_failed" });
    return;
  }
  navigator.geolocation.getCurrentPosition(
    ({ coords }) => {
      showPosition({ lat: coords.latitude, lng: coords.longitude }, coords.accuracy);
      dioxus.send({ type: "located" });
    },
    (error) => {
      dioxus.send({
        type: error.code === error.PERMISSION_DENIED ? "location_denied" : "location_failed",
      });
    },
    { enableHighAccuracy: true, timeout: 10000, maximumAge: 60000 },
  );
}

// 스크립트가 끝나면 Dioxus가 채널을 닫아 이후의 gm_authFailure를 받을 수 없다. 화면에서 지도가 사라질 때까지 명령을 기다린다.
while (true) {
  const command = await dioxus.recv();
  if (command.type === "show_position") {
    showPosition({ lat: command.lat, lng: command.lng }, command.accuracy);
  } else if (command.type === "locate_in_browser") {
    locateInBrowser();
  } else if (command.type === "show_listings") {
    showListings(command.listings);
  } else if (command.type === "clear_draft") {
    clearDraft();
  }
}
