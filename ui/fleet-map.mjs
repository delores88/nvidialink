import * as THREE from './vendor/three/three.module.min.js';
import {OrbitControls} from './vendor/three/OrbitControls.js';
import {startVisibleRenderLoop} from './vendor/visible-render-loop.mjs';

// The Devices page's live network. This laptop, where agents run, sits in front; every
// other device sits on an arc behind it. Each link's stream moves at a rate set by its
// measured speed, or its reported link rate when nothing was measured. Each active job is
// a particle in its kind's colour, placed by phase: heading out, running around its device,
// or coming back with results. Nothing here invents throughput, utilization or progress.
const MAX_MAPPED = 6;
const STATUS = {
  dark: {ready: '#c4ef87', busy: '#c4ef87', checking: '#8fd3e8', offline: '#e7c98a', attention: '#e7c98a', stale: '#e7c98a', paused: '#6f7d76', setup: '#8fd3e8'},
  light: {ready: '#4c7d1c', busy: '#4c7d1c', checking: '#1f7590', offline: '#946210', attention: '#946210', stale: '#946210', paused: '#8a958f', setup: '#1f7590'},
};
const LINK = {dark: '#74acff', light: '#2f5fb3'};

function glowTexture() {
  const canvas = document.createElement('canvas');
  canvas.width = canvas.height = 64;
  const g = canvas.getContext('2d');
  const r = g.createRadialGradient(32, 32, 0, 32, 32, 32);
  r.addColorStop(0, 'rgba(255,255,255,1)');
  r.addColorStop(0.22, 'rgba(255,255,255,0.7)');
  r.addColorStop(0.55, 'rgba(255,255,255,0.14)');
  r.addColorStop(1, 'rgba(255,255,255,0)');
  g.fillStyle = r;
  g.fillRect(0, 0, 64, 64);
  const texture = new THREE.CanvasTexture(canvas);
  texture.colorSpace = THREE.SRGBColorSpace;
  return texture;
}

// A stable pseudo-random sequence, so particle layouts do not jump between refreshes.
function seeded(seed) {
  let x = seed >>> 0 || 1;
  return () => ((x = (x * 1664525 + 1013904223) >>> 0) / 4294967296);
}

// Flow speed from Mbit/s on a log scale: 10 Mbit/s crawls, 1 Gbit/s and above runs.
function flowRate(mbps) {
  if (!Number.isFinite(mbps) || mbps <= 0) return 0;
  return Math.min(1, Math.max(0.08, Math.log10(mbps) / Math.log10(2500)));
}

// Screen-space layout, so the cards under each node never collide: devices spread evenly
// across the width (two staggered rows past four), this laptop front and centre.
export function screenLayout(count) {
  const rows = count > 4 ? 2 : 1;
  return {
    hub: {x: 0, y: -0.3},
    devices: Array.from({length: count}, (_, i) => ({
      x: count === 1 ? 0 : -0.74 + (1.48 * i) / (count - 1),
      y: rows === 2 ? (i % 2 ? 0.12 : 0.52) : 0.42,
    })),
  };
}

export function createFleetMap(host, {labelFor = () => null, onSelect = () => {}} = {}) {
  let renderer;
  try {
    renderer = new THREE.WebGLRenderer({antialias: true, alpha: true, powerPreference: 'low-power'});
  } catch {
    host.dataset.mode = 'grid';
    return {available: false, render() {}, resetView() {}, setActive() {}, dispose() {}};
  }
  renderer.setPixelRatio(Math.min(2, window.devicePixelRatio || 1));
  const canvas = renderer.domElement;
  canvas.className = 'fleet-canvas';
  canvas.setAttribute('aria-hidden', 'true');
  host.prepend(canvas);
  const scene = new THREE.Scene();
  // `home` decides the layout and never moves; `camera` is the one the owner drives.
  const home = new THREE.PerspectiveCamera(42, 2, 0.1, 200), camera = new THREE.PerspectiveCamera(42, 2, 0.1, 200);
  home.position.set(0, 6.5, 9);
  home.lookAt(0, 0, 0);
  camera.position.copy(home.position);
  const controls = new OrbitControls(camera, canvas);
  Object.assign(controls, {enableDamping: true, dampingFactor: 0.08, screenSpacePanning: true, minDistance: 4, maxDistance: 32, maxPolarAngle: Math.PI / 2.05, zoomSpeed: 0.8});
  controls.target.set(0, 0, 0);
  let steered = false, drawing = false;
  controls.addEventListener('start', () => { steered = true; });
  // Without a running loop (reduced motion), each move of the view draws one frame.
  controls.addEventListener('change', () => { if (!loop && !drawing) draw(performance.now()); });
  const glow = glowTexture();
  const world = new THREE.Group();
  scene.add(world);
  const ground = new THREE.Plane(new THREE.Vector3(0, 1, 0), 0), ray = new THREE.Raycaster();
  let HUB = new THREE.Vector3(0, 0, 3), lastData = null;
  // The world point on the ground plane that appears at this screen position.
  function onGround(x, y) {
    ray.setFromCamera(new THREE.Vector2(x, y), home);
    return ray.ray.intersectPlane(ground, new THREE.Vector3()) || new THREE.Vector3(x * 6, 0, -y * 6);
  }
  let built = [], nodes = [], streams = [], workers = [], stars = null;
  let loop = null, active = false, reduced = false, theme = 'dark';
  const started = performance.now();

  function material(color, size, light) {
    return new THREE.PointsMaterial({color: 0xffffff, size, map: glow, vertexColors: !color, transparent: true, depthWrite: false,
      blending: light ? THREE.NormalBlending : THREE.AdditiveBlending, opacity: light ? 0.85 : 1, ...(color ? {color: new THREE.Color(color)} : {})});
  }
  function track(object) {
    built.push(object);
    world.add(object);
    return object;
  }
  function clear() {
    for (const object of built) {
      world.remove(object);
      object.geometry?.dispose?.();
      object.material?.dispose?.();
    }
    built = [];
    nodes = [];
    streams = [];
    workers = [];
  }
  function sampler(curve) {
    const points = curve.getSpacedPoints(96);
    return (t, out) => {
      const x = Math.min(0.9999, Math.max(0, t)) * 96, i = Math.floor(x), f = x - i;
      return out.copy(points[i]).lerp(points[i + 1], f);
    };
  }

  function build(data) {
    clear();
    const light = theme === 'light', palette = STATUS[light ? 'light' : 'dark'];
    if (!stars) {
      const random = seeded(7), positions = new Float32Array(700 * 3);
      for (let i = 0; i < 700; i++) positions.set([(random() - 0.5) * 60, -8 + random() * 22, -40 + random() * 46], i * 3);
      const geometry = new THREE.BufferGeometry();
      geometry.setAttribute('position', new THREE.BufferAttribute(positions, 3));
      stars = new THREE.Points(geometry, material('#9fb8ad', 0.07, false));
      scene.add(stars);
    }
    stars.visible = !light;
    const mapped = data.devices.slice(0, MAX_MAPPED), layout = screenLayout(mapped.length);
    HUB = onGround(layout.hub.x, layout.hub.y);
    const positions = layout.devices.map(spot => onGround(spot.x, spot.y));
    const hubColor = palette.ready;
    const hub = track(new THREE.Sprite(new THREE.SpriteMaterial({map: glow, color: hubColor, transparent: true, depthWrite: false, blending: light ? THREE.NormalBlending : THREE.AdditiveBlending})));
    hub.position.copy(HUB);
    hub.scale.setScalar(1.25);
    nodes.push({id: data.hub.id, position: HUB.clone(), sprite: hub, base: 1.25});
    mapped.forEach((device, index) => {
      const position = positions[index], color = palette[device.status] || palette.paused;
      const size = 0.9 + Math.min(0.9, Math.max(0, Math.log2((device.memory || 16) / 16)) * 0.28);
      const sprite = track(new THREE.Sprite(new THREE.SpriteMaterial({map: glow, color, transparent: true, depthWrite: false, blending: light ? THREE.NormalBlending : THREE.AdditiveBlending})));
      sprite.position.copy(position);
      sprite.scale.setScalar(size);
      const node = {id: device.id, position, sprite, base: size, ring: null};
      if (device.gpu) {
        const ring = track(new THREE.Mesh(new THREE.RingGeometry(size * 0.48, size * 0.53, 64), new THREE.MeshBasicMaterial({color: '#b798f8', transparent: true, opacity: light ? 0.7 : 0.55, side: THREE.DoubleSide, depthWrite: false})));
        ring.position.copy(position);
        ring.rotation.x = -Math.PI / 2.4;
        node.ring = ring;
      }
      nodes.push(node);
      // The link: a faint arc, and a stream whose speed follows the link's real rate.
      const control = HUB.clone().lerp(position, 0.5).add(new THREE.Vector3(0, 1.4, 0));
      const curve = new THREE.QuadraticBezierCurve3(HUB.clone(), control, position.clone()), at = sampler(curve);
      const lineGeometry = new THREE.BufferGeometry().setFromPoints(curve.getPoints(64));
      track(new THREE.Line(lineGeometry, new THREE.LineBasicMaterial({color: LINK[light ? 'light' : 'dark'], transparent: true, opacity: device.status === 'paused' ? 0.08 : 0.22, depthWrite: false})));
      const rate = ['offline', 'stale', 'paused', 'setup'].includes(device.status) ? 0 : flowRate(device.mbps);
      const count = rate ? Math.round(14 + rate * 34) : 10, random = seeded(index + 11);
      const geometry = new THREE.BufferGeometry(), points = new Float32Array(count * 3), offsets = Float32Array.from({length: count}, () => random());
      geometry.setAttribute('position', new THREE.BufferAttribute(points, 3));
      const stream = track(new THREE.Points(geometry, material(rate ? LINK[light ? 'light' : 'dark'] : palette.paused, rate ? 0.16 : 0.1, light)));
      streams.push({stream, at, offsets, speed: rate ? 0.05 + rate * 0.32 : 0});
      // Jobs: one particle each, in its kind's colour, where it is.
      for (const [kind, phases] of Object.entries(device.work || {})) {
        phases.slice(0, 40).forEach((phase, n) => workers.push({at, position, color: data.kinds[kind]?.color || '#9aa5a0', phase, seed: seeded(index * 97 + n * 13 + kind.length)(), size}));
      }
    });
    // Jobs running on this laptop circle the hub.
    for (const [kind, phases] of Object.entries(data.hub.work || {})) {
      phases.slice(0, 40).forEach((phase, n) => workers.push({at: null, position: HUB, color: data.kinds[kind]?.color || '#9aa5a0', phase: 'running', seed: seeded(n * 31 + kind.length)(), size: 1.25}));
    }
    if (workers.length) {
      const geometry = new THREE.BufferGeometry();
      geometry.setAttribute('position', new THREE.BufferAttribute(new Float32Array(workers.length * 3), 3));
      geometry.setAttribute('color', new THREE.BufferAttribute(Float32Array.from(workers.flatMap(worker => new THREE.Color(worker.color).toArray())), 3));
      const points = track(new THREE.Points(geometry, material(null, 0.32, light)));
      points.userData.workers = true;
      workers.points = points;
    }
    host.dataset.devices = String(mapped.length);
  }

  const point = new THREE.Vector3(), scratch = new THREE.Vector3();
  function place(seconds) {
    for (const {stream, at, offsets, speed} of streams) {
      const positions = stream.geometry.attributes.position;
      for (let i = 0; i < offsets.length; i++) {
        at((offsets[i] + seconds * speed) % 1, point);
        positions.setXYZ(i, point.x, point.y, point.z);
      }
      positions.needsUpdate = true;
    }
    if (workers.points) {
      const positions = workers.points.geometry.attributes.position;
      workers.forEach((worker, i) => {
        if (worker.phase === 'running' || !worker.at) {
          const angle = worker.seed * Math.PI * 2 + seconds * (0.5 + worker.seed * 0.6), radius = worker.size * (0.55 + worker.seed * 0.35);
          point.set(worker.position.x + Math.cos(angle) * radius, worker.position.y + Math.sin(angle * 2) * 0.12 + 0.05, worker.position.z + Math.sin(angle) * radius * 0.6);
        } else if (worker.phase === 'waiting') {
          worker.at(0.06 + ((worker.seed + seconds * 0.05) % 1) * 0.2, point);
        } else {
          worker.at(1 - ((worker.seed + seconds * 0.22) % 1), point);
        }
        positions.setXYZ(i, point.x, point.y, point.z);
      });
      positions.needsUpdate = true;
    }
    for (const node of nodes) {
      const pulse = 1 + Math.sin(seconds * 1.6 + node.position.x) * 0.04;
      node.sprite.scale.setScalar(node.base * pulse);
      if (node.ring) node.ring.rotation.z = seconds * 0.35;
    }
    if (stars) stars.rotation.y = seconds * 0.006;
  }

  function layoutLabels() {
    const width = canvas.clientWidth, height = canvas.clientHeight;
    for (const node of nodes) {
      const label = labelFor(node.id);
      if (!label) continue;
      scratch.copy(node.position).add(new THREE.Vector3(0, -node.base * 0.62, 0)).applyMatrix4(world.matrixWorld).project(camera);
      const shown = scratch.z < 1 && Math.abs(scratch.x) < 1.15 && Math.abs(scratch.y) < 1.15;
      label.style.visibility = shown ? 'visible' : 'hidden';
      label.style.left = `${((scratch.x + 1) / 2) * width}px`;
      label.style.top = `${((1 - scratch.y) / 2) * height}px`;
    }
  }

  function draw(now) {
    const seconds = reduced ? 0 : (now - started) / 1000;
    drawing = true;
    // A slow sway keeps depth readable until the owner takes the controls.
    world.rotation.y = reduced || steered ? 0 : Math.sin(seconds * 0.16) * 0.05;
    controls.update();
    place(seconds);
    renderer.render(scene, camera);
    layoutLabels();
    drawing = false;
  }
  // Frames run only while the page is visible and the map is on screen.
  function animate() {
    const live = active && !reduced;
    host.dataset.motion = live ? 'live' : 'static';
    if (live && !loop) loop = startVisibleRenderLoop(host, draw);
    if (!live && loop) {
      loop.dispose();
      loop = null;
    }
  }
  function resize() {
    const width = host.clientWidth, height = host.clientHeight;
    if (!width || !height) return;
    renderer.setSize(width, height, false);
    for (const view of [home, camera]) {
      view.aspect = width / height;
      view.updateProjectionMatrix();
    }
    home.updateMatrixWorld();
    if (lastData) build(lastData);
    draw(performance.now());
  }
  const observer = new ResizeObserver(resize);
  observer.observe(host);
  // A click (not a drag) on a device's glow opens that device.
  let pressed = null;
  canvas.addEventListener('pointerdown', event => { pressed = {x: event.clientX, y: event.clientY}; });
  canvas.addEventListener('pointerup', event => {
    if (!pressed || Math.hypot(event.clientX - pressed.x, event.clientY - pressed.y) > 5) return;
    const rect = canvas.getBoundingClientRect();
    ray.setFromCamera(new THREE.Vector2(((event.clientX - rect.left) / rect.width) * 2 - 1, -((event.clientY - rect.top) / rect.height) * 2 + 1), camera);
    const hit = ray.intersectObjects(nodes.map(node => node.sprite))[0];
    const node = hit && nodes.find(item => item.sprite === hit.object);
    if (node) onSelect(node.id);
  });
  host.dataset.mode = 'map';

  return {
    available: true,
    render(data, options = {}) {
      reduced = !!options.reduced;
      theme = options.theme === 'light' ? 'light' : 'dark';
      controls.enableDamping = !reduced;
      host.dataset.mode = data.devices.length > MAX_MAPPED ? 'grid' : 'map';
      lastData = data;
      resize();
      animate();
    },
    resetView() {
      steered = false;
      camera.position.copy(home.position);
      controls.target.set(0, 0, 0);
      controls.update();
      draw(performance.now());
    },
    setActive(on) {
      active = on;
      if (active) resize();
      animate();
    },
    dispose() {
      active = false;
      animate();
      controls.dispose();
      observer.disconnect();
      clear();
      stars?.geometry.dispose();
      stars?.material.dispose();
      glow.dispose();
      renderer.dispose();
      canvas.remove();
    },
  };
}
