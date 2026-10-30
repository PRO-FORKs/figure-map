// Google Maps JavaScript API 브리지. Dioxus webview에서 Rust 바인딩 없이 지도를 띄우는 유일한 경로라 JS로 둔다.
// Rust → JS: 처음 { key, elementId, fallback: { lat, lng } }, 이후 명령
//   { type: "show_position", lat, lng, accuracy } | { type: "locate_in_browser" }
//   | { type: "show_listings", listings: [{ key, ids, lat, lng, title, count, image }] } | { type: "clear_draft" }
// JS → Rust: { type: "loaded" | "located" | "location_denied" | "location_failed" | "load_failed" | "auth_failed" }
//   | { type: "clicked", lat, lng } | { type: "listing_clicked", ids }
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

let Map, Circle, Marker, Point, Size;
try {
  await window.__sharedMapLoader;
  ({ Map, Circle } = await google.maps.importLibrary("maps"));
  // AdvancedMarkerElement는 Map ID가 필요해 아직 설정이 없는 지금은 기본 Marker를 쓴다.
  ({ Marker } = await google.maps.importLibrary("marker"));
  ({ Point, Size } = await google.maps.importLibrary("core"));
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

const ICON_SIZE = 56;
const PHOTO_SIZE = 44;
const iconCache = new globalThis.Map();

// 기본 Marker는 HTML 내용을 못 넣으므로 사진과 +N 배지를 캔버스로 합쳐 아이콘 이미지로 쓴다.
function photoIcon(image, extra) {
  const cacheKey = image + "|" + extra;
  let icon = iconCache.get(cacheKey);
  if (!icon) {
    icon = drawPhotoIcon(image, extra);
    iconCache.set(cacheKey, icon);
    icon.catch(() => iconCache.delete(cacheKey));
  }
  return icon;
}

function drawPhotoIcon(image, extra) {
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.onerror = reject;
    img.onload = () => {
      const scale = window.devicePixelRatio || 1;
      const canvas = document.createElement("canvas");
      canvas.width = ICON_SIZE * scale;
      canvas.height = ICON_SIZE * scale;
      const ctx = canvas.getContext("2d");
      ctx.scale(scale, scale);

      const x = (ICON_SIZE - PHOTO_SIZE) / 2;
      const y = ICON_SIZE - PHOTO_SIZE - 2;
      ctx.fillStyle = "#ffffff";
      ctx.shadowColor = "rgba(0, 0, 0, 0.35)";
      ctx.shadowBlur = 3;
      ctx.beginPath();
      ctx.roundRect(x - 2, y - 2, PHOTO_SIZE + 4, PHOTO_SIZE + 4, 8);
      ctx.fill();
      ctx.shadowColor = "transparent";

      // 가운데를 정사각형으로 잘라 채운다.
      const side = Math.min(img.width, img.height);
      ctx.save();
      ctx.beginPath();
      ctx.roundRect(x, y, PHOTO_SIZE, PHOTO_SIZE, 6);
      ctx.clip();
      ctx.drawImage(img, (img.width - side) / 2, (img.height - side) / 2, side, side, x, y, PHOTO_SIZE, PHOTO_SIZE);
      ctx.restore();

      if (extra > 0) {
        const text = "+" + extra;
        ctx.font = "bold 11px sans-serif";
        const width = Math.max(20, ctx.measureText(text).width + 10);
        const right = ICON_SIZE - 1;
        ctx.fillStyle = "#d93025";
        ctx.strokeStyle = "#ffffff";
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.roundRect(right - width, 1, width, 20, 10);
        ctx.fill();
        ctx.stroke();
        ctx.fillStyle = "#ffffff";
        ctx.textAlign = "center";
        ctx.textBaseline = "middle";
        ctx.fillText(text, right - width / 2, 11.5);
      }
      resolve({
        url: canvas.toDataURL("image/png"),
        scaledSize: new Size(ICON_SIZE, ICON_SIZE),
        anchor: new Point(ICON_SIZE / 2, ICON_SIZE),
      });
    };
    img.src = image;
  });
}

function countLabel(count, image) {
  return !image && count > 1 ? { text: "+" + (count - 1), color: "#ffffff", fontWeight: "bold" } : null;
}

function applyAppearance(entry, { image, count }) {
  const appearance = (image || "") + "|" + count;
  if (entry.appearance === appearance) {
    return;
  }
  entry.appearance = appearance;
  entry.marker.setLabel(countLabel(count, image));
  if (!image) {
    entry.marker.setIcon(null);
    return;
  }
  photoIcon(image, count - 1).then(
    (icon) => {
      // 기다리는 동안 묶음이 바뀌었으면 오래된 아이콘을 덮어쓰지 않는다.
      if (entry.appearance === appearance) {
        entry.marker.setIcon(icon);
      }
    },
    () => {
      if (entry.appearance === appearance) {
        entry.marker.setLabel(countLabel(count, null));
      }
    },
  );
}

function showListings(listings) {
  const keys = new Set(listings.map((listing) => listing.key));
  for (const [key, entry] of listingMarkers) {
    if (!keys.has(key)) {
      entry.marker.setMap(null);
      listingMarkers.delete(key);
    }
  }
  for (const listing of listings) {
    const { key, ids, lat, lng, title } = listing;
    let entry = listingMarkers.get(key);
    if (entry) {
      entry.marker.setPosition({ lat, lng });
      entry.marker.setTitle(title);
    } else {
      entry = { marker: new Marker({ map, position: { lat, lng }, title }), ids, appearance: null };
      entry.marker.addListener("click", () => dioxus.send({ type: "listing_clicked", ids: entry.ids }));
      listingMarkers.set(key, entry);
    }
    entry.ids = ids;
    applyAppearance(entry, listing);
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
