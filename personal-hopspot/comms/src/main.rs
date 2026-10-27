#[cfg(not(target_arch = "wasm32"))]
mod banner;
#[cfg(target_os = "android")]
mod camera;
#[cfg(not(target_arch = "wasm32"))]
mod chime;
#[cfg(target_os = "android")]
mod lan;
#[cfg(target_os = "android")]
mod links;
mod location;
mod lxmf;
#[cfg(not(target_arch = "wasm32"))]
mod mailbox;
mod model;
mod scan;
#[cfg(not(target_arch = "wasm32"))]
mod session;
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod viewfinder;
#[cfg(target_os = "windows")]
mod viewfinder_windows;
#[cfg(target_os = "windows")]
use viewfinder_windows as viewfinder;
#[cfg(target_os = "macos")]
mod film;
#[cfg(any(target_os = "macos", target_os = "android"))]
mod video;
#[cfg(not(target_arch = "wasm32"))]
mod voice;

use dioxus::prelude::*;
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
use scan::contact_from_rgba;
use scan::{contact_from_code, contact_from_image};

#[cfg(not(target_arch = "wasm32"))]
use model::apply_reaction;
#[cfg(not(target_arch = "wasm32"))]
use model::note_from_them;
#[cfg(not(target_arch = "wasm32"))]
use model::remember_call;
#[cfg(target_arch = "wasm32")]
use model::sample_people;
use model::{
    add_person, can_call, code_mark, contact_code, file_choice, file_name_of, file_too_big,
    group_by_day, has_unread, map_window, mark_read, offered_ways, path_label, react_to_note,
    remove_person, rename_person, retry_note, shown_words, write_file_note, write_note,
    write_placed_note, NoteKind, OfferedWays, Page, Person, Place, Reach, Reaction, PLACEHOLDER,
    YOUR_PUBLIC_KEY,
};
#[cfg(not(target_arch = "wasm32"))]
use model::{fail_note, fail_their_file, land_their_file, post_note, settle_note};

const KEEP_KEYBOARD: &str = r#"
if (!window.__touchbaseKeepKeyboard) {
  window.__touchbaseKeepKeyboard = true;
  var iphone = /iPhone|iPad|iPod/.test(navigator.userAgent);
  if (iphone) {
    var keep = function (event) {
      var node = event.target;
      if (!node || !node.closest) return;
      if (node.closest(".plus, .embed-list")) event.preventDefault();
    };
    document.addEventListener("touchstart", keep, {capture: true, passive: false});
    document.addEventListener("mousedown", keep, true);
  }
}
"#;

// Android takes a downward drag at the top of the list as scrolling. This follows
// that drag and opens the seam without calling preventDefault, so a sideways
// slide can still reveal rename and remove. 56 matches `SEAM`.
#[cfg(target_os = "android")]
const SEAM_TOUCH: &str = r#"
if (!window.__touchbaseSeam) {
  window.__touchbaseSeam = true;
  var startX = 0;
  var startY = 0;
  var active = false;
  var sideways = false;
  var opened = false;
  var root = document.documentElement;
  var maxSeam = 56;
  function pageOf(event) {
    var node = event.target;
    if (!node || !node.closest) return null;
    return node.closest(".page.people");
  }
  function closeSeam() {
    active = false;
    sideways = false;
    if (!opened) return;
    opened = false;
    root.style.setProperty("--seam-move", "180ms ease");
    requestAnimationFrame(function () {
      root.style.setProperty("--seam", "0px");
      setTimeout(function () {
        root.style.removeProperty("--seam");
        root.style.removeProperty("--seam-move");
      }, 220);
    });
  }
  document.addEventListener("touchstart", function (event) {
    if (event.touches.length !== 1) return;
    var page = pageOf(event);
    sideways = false;
    if (!page || page.scrollTop > 1) {
      active = false;
      return;
    }
    startX = event.touches[0].clientX;
    startY = event.touches[0].clientY;
    active = true;
  }, {capture: true, passive: true});
  document.addEventListener("touchmove", function (event) {
    if (!active || sideways || event.touches.length !== 1) return;
    var dx = event.touches[0].clientX - startX;
    var dy = event.touches[0].clientY - startY;
    if (Math.abs(dx) > 8 && Math.abs(dx) >= dy) {
      sideways = true;
      return;
    }
    if (dy < 18 || dy <= Math.abs(dx)) return;
    opened = true;
    root.style.setProperty("--seam-move", "none");
    root.style.setProperty("--seam", Math.min(dy, maxSeam) + "px");
  }, {capture: true, passive: true});
  document.addEventListener("touchend", closeSeam, {capture: true, passive: true});
  document.addEventListener("touchcancel", closeSeam, {capture: true, passive: true});
}
"#;

const PAPER: &str = r#"
:root {
  color-scheme: light;
  --paper: #f3efe4;
  --ink: #2b2722;
  --note: #5c564e;
  --pencil: #6e685f;
  --rule: rgba(92, 78, 58, 0.28);
  --hand: "Kalam", Noteworthy, "Segoe Print", cursive;
  --their-hand: "Patrick Hand", "Segoe Script", cursive;
  --book: "Iowan Old Style", Palatino, "Palatino Linotype", "Book Antiqua", "Libre Baskerville", Georgia, serif;
}
* { box-sizing: border-box; }
html, body, #main {
  margin: 0;
  height: 100%;
  background: var(--paper);
  color: var(--ink);
}
button, input, textarea {
  font: inherit;
  color: inherit;
  background: transparent;
  border: 0;
  padding: 0;
}
button { cursor: pointer; }
button:focus-visible, input:focus-visible, textarea:focus-visible {
  outline: 1px solid var(--pencil);
  outline-offset: 3px;
}
.notebook {
  height: 100%;
  overflow: hidden;
  font-family: var(--book);
  background: var(--paper);
}
.page {
  --edge-left: max(44px, env(safe-area-inset-left));
  --edge-right: max(20px, env(safe-area-inset-right));
  height: 100%;
  overflow: auto;
  padding:
    calc(max(18px, env(safe-area-inset-top)) + 22px)
    var(--edge-right)
    max(32px, env(safe-area-inset-bottom))
    var(--edge-left);
  background: var(--paper);
}
.page.people {
  position: relative;
  overscroll-behavior-y: none;
  overflow-anchor: none;
}
.seam {
  position: absolute;
  top: max(14px, env(safe-area-inset-top));
  left: 0;
  right: 0;
  height: var(--seam, 0px);
  overflow: hidden;
  display: flex;
  align-items: flex-end;
  justify-content: center;
  transition: height 180ms ease;
  pointer-events: none;
}
.people-body {
  transform: translateY(var(--seam, 0px));
  transition: transform 180ms ease;
}
.page.people.pulling .seam,
.page.people.pulling .people-body { transition: none; }
.seam span {
  padding-bottom: 10px;
  color: var(--pencil);
  font-family: var(--book);
  font-size: 15px;
  line-height: 1;
  letter-spacing: 0.08em;
  user-select: none;
}
.wires {
  position: fixed;
  z-index: 3;
  top: max(14px, env(safe-area-inset-top));
  left: 0;
  right: 0;
  display: flex;
  justify-content: center;
  gap: 0.7em;
  margin: 0;
  pointer-events: none;
  font-family: var(--book);
  font-size: 15px;
  line-height: 1;
  letter-spacing: 0.08em;
  transform: translateY(var(--seam, 0px));
  transition: transform var(--seam-move, 180ms ease);
}
.wires .live { color: var(--ink); }
.wires .quiet { color: var(--pencil); opacity: 0.35; }
.incoming {
  position: fixed;
  z-index: 4;
  top: calc(max(14px, env(safe-area-inset-top)) + 22px);
  left: 0;
  right: 0;
  display: flex;
  justify-content: flex-end;
  align-items: baseline;
  gap: 18px;
  margin: 0;
  padding: 2px max(20px, env(safe-area-inset-right)) 6px max(20px, env(safe-area-inset-left));
  background: var(--paper);
  transform: translateY(var(--seam, 0px));
  transition: transform var(--seam-move, 180ms ease);
}
.incoming p {
  margin: 0;
  color: var(--ink);
  font-size: 22px;
  line-height: 1.2;
}
.incoming button {
  color: var(--pencil);
  font-family: var(--book);
  font-size: 16px;
  line-height: 1;
  text-decoration: underline;
  text-underline-offset: 4px;
  min-height: 44px;
}
.hand { font-family: var(--hand); }
.actions {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 6px;
}
.action {
  color: var(--pencil);
  font-size: 16px;
  line-height: 1;
  text-decoration: underline;
  text-underline-offset: 4px;
  text-decoration-thickness: 1px;
  min-height: 44px;
  padding: 12px 0;
}
.empty-book {
  margin: 18vh calc(-1 * var(--edge-right)) 0 calc(-1 * var(--edge-left));
  padding: 0 36px;
  text-align: center;
  color: var(--ink);
  font-size: 28px;
  line-height: 1.35;
}
.names { list-style: none; margin: 12px 0 0; padding: 0; }
.names li { border-bottom: 1px solid var(--rule); }
.names .who, .save {
  display: block;
  width: 100%;
  text-align: left;
  font-size: 28px;
  line-height: 1.45;
  padding: 10px 0 8px;
}
.name-swipe {
  display: flex;
  align-items: stretch;
  overflow: hidden;
  touch-action: pan-y;
}
.names .name-swipe > .who {
  flex: 1 1 auto;
  width: auto;
  min-width: 0;
  user-select: none;
}
.names .fresh { margin-right: 0.35em; }
.name-config, .name-remove {
  overflow: hidden;
  display: flex;
  flex: 0 0 0px;
  transition: flex-basis 180ms ease;
}
.name-config { flex-basis: var(--config, 0px); justify-content: flex-end; }
.name-remove { flex-basis: var(--remove, 0px); justify-content: flex-end; }
.name-config button, .name-remove button {
  display: flex;
  align-items: center;
  flex: 0 0 84px;
  width: 84px;
  height: 100%;
  box-sizing: border-box;
  color: var(--pencil);
  font-family: var(--book);
  font-size: 16px;
  line-height: 1;
  text-decoration: underline;
  text-underline-offset: 4px;
}
.name-config button { justify-content: flex-end; padding: 0 12px 0 0; }
.name-remove button { justify-content: flex-end; padding: 0 0 0 12px; }
.name-swipe.live .name-config,
.name-swipe.live .name-remove { transition: none; }
input.who.rename {
  border-bottom: 1px solid var(--pencil);
}
.confirm-remove { padding: 12px 0 8px; }
.confirm-remove p {
  margin: 0;
  color: var(--ink);
  font-family: var(--book);
  font-size: 16px;
  line-height: 1.4;
}
.confirm-actions { display: flex; gap: 22px; }
.confirm-actions button {
  width: auto;
  color: var(--pencil);
  font-family: var(--book);
  font-size: 16px;
  line-height: 1;
  text-decoration: underline;
  text-underline-offset: 4px;
  min-height: 44px;
  padding: 12px 0;
}
.save {
  color: var(--pencil);
  font-size: 18px;
  text-decoration: underline;
  text-underline-offset: 4px;
  width: auto;
}
.sheet {
  position: fixed;
  inset: 0;
  overflow: hidden;
  background: var(--paper);
  transform: translateX(100%);
  transition: transform 240ms ease;
  pointer-events: none;
}
.journal {
  --edge-right: var(--edge-left);
  display: flex;
  flex-direction: column;
  overflow: hidden;
  padding: 0;
}
.journal .title-row {
  flex: none;
  margin: 0;
  padding:
    calc(max(14px, env(safe-area-inset-top)) + 18px)
    var(--edge-right)
    6px
    var(--edge-left);
  background: var(--paper);
}
.journal .title-row h1 { line-height: 1; }
.journal .back { margin-top: 1px; }
.journal .log {
  flex: 1 1 auto;
  min-height: 0;
  display: flex;
  flex-direction: column;
  overflow: auto;
  padding:
    4px
    var(--edge-right)
    0
    var(--edge-left);
  background-color: var(--paper);
  background-image: url("data:image/png;base64,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");
  background-repeat: repeat;
  background-attachment: local;
}
.dock {
  position: sticky;
  bottom: 0;
  z-index: 2;
  margin-top: 8px;
  padding-top: 6px;
  padding-bottom: max(16px, env(safe-area-inset-bottom));
  background: var(--paper);
}
.sheet.open {
  transform: translateX(0);
  pointer-events: auto;
}
.title-row {
  display: flex;
  align-items: flex-start;
  gap: 14px;
  margin: 0 0 8px;
}
.who { min-width: 0; }
.title-row h1 {
  margin: 0;
  font-size: 30px;
  font-weight: 500;
}
.back { font-size: 26px; line-height: 1; margin-top: 6px; }
.via {
  margin: 1px 0 0;
  color: var(--pencil);
  font-family: var(--book);
  font-size: 13px;
  line-height: 1.2;
}
.thread { list-style: none; margin: 10px 0 0; padding: 0; }
.day {
  display: flex;
  align-items: center;
  gap: 12px;
  margin: 16px 0 12px;
  color: var(--pencil);
  font-size: 14px;
}
.day::before, .day::after {
  content: "";
  flex: 1;
  height: 1px;
  background: var(--rule);
}
.swipe {
  display: flex;
  align-items: flex-start;
  overflow: hidden;
  touch-action: pan-y;
}
.words {
  flex: 1 1 auto;
  min-width: 0;
  margin: 0;
  font-size: 22px;
  line-height: 1.35;
  overflow-wrap: anywhere;
  word-break: break-word;
  -webkit-user-select: text;
  user-select: text;
  touch-action: auto;
}
.words.hand { font-size: 19.5px; color: var(--note); }
.theirs .words.hand { font-family: var(--their-hand); font-size: 22px; }
.mine .note-body { margin-left: var(--edge-left); }
.theirs .note-body { margin-right: var(--edge-left); }
.mine .pack {
  display: flex;
  flex-direction: column;
  align-items: stretch;
  width: fit-content;
  max-width: 100%;
  margin-left: auto;
}
.mine .pack .words.hand {
  flex: none;
  width: auto;
  max-width: 100%;
  text-align: left;
}
.mine .reaction { justify-content: flex-end; }
.mine .place { margin-left: auto; }
.mine .map-credit { text-align: right; }
.voice .words, .video .words {
  width: fit-content;
  max-width: 100%;
  padding: 3px 8px;
  border-radius: 8px;
  background: color-mix(in srgb, var(--pencil) 8%, var(--paper));
  color: var(--pencil);
  font-family: var(--book);
  font-size: 14px;
  line-height: 1.25;
}
.waiting .words.hand { color: #b1a89c; }
.reaction {
  display: flex;
  justify-content: flex-start;
  margin-top: 0;
  color: var(--ink);
}
.reaction svg,
.reacts svg {
  display: block;
  box-sizing: content-box;
  width: 26px;
  height: 26px;
  padding: 3px;
  background: var(--pencil);
  color: var(--paper);
  border-radius: 8px;
}
.failed {
  display: flex;
  justify-content: flex-end;
  margin-top: 0;
}
.theirs .failed { justify-content: flex-start; }
.file-line {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: 8px;
  max-width: 100%;
}
.file-line .attached {
  display: inline;
  width: auto;
  max-width: 100%;
}
.toast {
  margin: 0;
  pointer-events: none;
  font-family: var(--book);
  font-size: 14px;
  line-height: 1;
  color: var(--paper);
  background: var(--pencil);
  border-radius: 8px;
  padding: 6px 8px;
  white-space: nowrap;
}
.failed button,
.posted span {
  font-family: var(--book);
  font-size: 14px;
  line-height: 1;
  color: var(--paper);
  background: var(--pencil);
  border-radius: 8px;
  padding: 6px 8px;
}
.posted {
  display: flex;
  justify-content: flex-end;
  margin-top: 0;
}
.meter {
  width: 148px;
  max-width: 100%;
  height: 4px;
  margin-top: 8px;
  border-radius: 999px;
  background: color-mix(in srgb, var(--pencil) 16%, var(--paper));
  overflow: hidden;
}
.meter span {
  display: block;
  height: 100%;
  background: var(--pencil);
  border-radius: inherit;
}
.mine .meter { margin-left: auto; }
.reacts {
  display: flex;
  flex-wrap: wrap;
  align-items: flex-start;
  gap: 2px;
  margin-top: 8px;
}
.reacts button {
  display: inline-flex;
  align-items: flex-start;
  justify-content: center;
  color: var(--ink);
  line-height: 1;
  min-width: 36px;
  min-height: 36px;
  padding: 0;
  opacity: 0.4;
}
.reacts button.chosen { opacity: 1; }
.reacts button.copy {
  align-items: center;
  align-self: flex-start;
  opacity: 1;
  box-sizing: content-box;
  height: 26px;
  min-height: 26px;
  min-width: 0;
  padding: 3px 8px;
  font-family: var(--book);
  font-size: 14px;
  line-height: 1;
  color: var(--paper);
  background: var(--pencil);
  border-radius: 8px;
}
.when {
  flex: 0 0 var(--reveal, 0px);
  overflow: hidden;
  margin: 0 0 16px;
  transition: flex-basis 180ms ease;
}
.when span {
  display: block;
  width: 72px;
  padding: 6px 0 0 12px;
  box-sizing: border-box;
  text-align: right;
  color: var(--pencil);
  font-size: 15px;
  line-height: 1.35;
  font-variant-numeric: tabular-nums;
  transform: translateX(calc(72px - var(--reveal, 0px)));
  transition: transform 180ms ease;
}
.swipe.live .when,
.swipe.live .when span { transition: none; }
.call-line { margin-top: 8px; }
.call-line .hint {
  margin: 0 0 4px;
  text-align: right;
}
.actions {
  display: flex;
  align-items: center;
  gap: 16px;
}
.offers {
  display: flex;
  justify-content: flex-start;
  flex: 0 0 auto;
  gap: 16px;
}
.offers button, .send {
  color: var(--pencil);
  font-size: 16px;
  line-height: 1;
  text-decoration: underline;
  text-underline-offset: 4px;
  min-height: 44px;
  padding: 12px 0;
}
.send { margin-left: auto; }
.film {
  display: block;
  width: auto;
  height: auto;
  max-width: min(100%, 240px);
  max-height: 240px;
  margin: 8px 0 0 auto;
  object-fit: contain;
  background: #e7e1d4;
}
.photo {
  position: relative;
  display: inline-block;
  color: var(--pencil);
  font-size: 16px;
  line-height: 1;
  text-decoration: underline;
  text-underline-offset: 4px;
  min-height: 44px;
  padding: 12px 0;
  cursor: pointer;
}
.photo input {
  position: absolute;
  inset: 0;
  opacity: 0;
  cursor: pointer;
}
.finder { margin: 4px 0 8px; }
.finder .well {
  position: relative;
  display: flex;
  align-items: center;
  justify-content: center;
  background: #1c1a17;
  aspect-ratio: 16 / 9;
  overflow: hidden;
}
.finder .opening {
  margin: 0;
  color: var(--paper);
  font-size: 15px;
}
.finder img {
  display: block;
  width: 100%;
  height: 100%;
  object-fit: cover;
  transform: scaleX(-1);
}
.finder.rear .well { aspect-ratio: 3 / 4; }
.finder.rear img { transform: none; }
.finder .well::after {
  content: "";
  position: absolute;
  left: 14%;
  right: 14%;
  top: 12%;
  bottom: 12%;
  border: 1px solid var(--paper);
  border-radius: 8px;
  pointer-events: none;
}
.finder .aim {
  margin: 12px 0 0;
  text-align: center;
  color: var(--pencil);
  font-size: 15px;
}
.hint, .notice {
  margin: 0 0 18px;
  color: var(--pencil);
  font-size: 15px;
}
.composer {
  display: grid;
  margin-top: 8px;
  border-bottom: 1px solid var(--rule);
}
.composer::after {
  content: attr(data-value) " ";
  visibility: hidden;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  word-break: break-word;
  grid-area: 1 / 1;
  min-width: 0;
  max-height: calc(1.35em * 5);
  overflow: hidden;
  font-family: var(--hand);
  font-size: 19.5px;
  line-height: 1.35;
}
.composer textarea {
  grid-area: 1 / 1;
  min-width: 0;
  width: 100%;
  height: 100%;
  min-height: 1.35em;
  max-height: calc(1.35em * 5);
  resize: none;
  overflow-x: hidden;
  overflow-y: auto;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  word-break: break-word;
  font-family: var(--hand);
  font-size: 19.5px;
  line-height: 1.35;
  text-align: left;
  color: var(--note);
}
.composer textarea::placeholder { color: var(--pencil); font-family: var(--hand); }
.write-row {
  position: relative;
  display: flex;
  flex-direction: column;
}
.write-row .composer {
  width: 100%;
  min-width: 0;
  margin-top: 0;
}
.note-line { position: relative; }
.embed {
  position: absolute;
  left: calc(-1 * var(--edge-left));
  bottom: 0;
  width: var(--edge-left);
  display: flex;
  justify-content: center;
}
.plus {
  min-width: 36px;
  min-height: 44px;
  font-size: 28px;
  line-height: 1;
  color: var(--pencil);
  text-decoration: none;
  cursor: pointer;
  background: transparent;
  border: 0;
  padding: 0;
  -webkit-user-select: none;
  user-select: none;
}
.embed-list {
  position: absolute;
  left: 0;
  bottom: calc(100% + 4px);
  min-width: 148px;
  background: var(--paper);
  border: 1px solid var(--rule);
  z-index: 5;
}
.embed-list .choice {
  position: relative;
  display: block;
  width: 100%;
  text-align: left;
  padding: 12px 14px;
  font-family: var(--book);
  font-size: 16px;
  line-height: 1;
  text-decoration: underline;
  text-underline-offset: 3px;
  min-height: 44px;
  cursor: pointer;
  background: transparent;
  border: 0;
  color: inherit;
  -webkit-user-select: none;
  user-select: none;
}
.embed-list .choice input {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  opacity: 0;
  cursor: pointer;
}
.attached {
  margin: 4px 0 0;
  font-family: var(--book);
  font-size: 15px;
  line-height: 1.3;
  color: var(--pencil);
  word-break: break-all;
}
button.attached {
  display: block;
  padding: 0;
  background: transparent;
  border: 0;
  text-align: left;
  cursor: pointer;
  text-decoration: underline;
  text-underline-offset: 3px;
}
.note-body {
  flex: 1 1 auto;
  min-width: 0;
  margin: 0 0 16px;
}
.place {
  position: relative;
  overflow: hidden;
  margin: 0 0 8px;
  background: rgba(92, 78, 58, 0.08);
  cursor: pointer;
}
.place .tile {
  position: absolute;
  max-width: none;
}
.pin {
  position: absolute;
  z-index: 1;
  left: 50%;
  top: 50%;
  width: 12px;
  height: 12px;
  margin: -6px 0 0 -6px;
  border-radius: 50%;
  background: var(--ink);
  box-shadow: 0 0 0 2px var(--paper);
}
.map-credit {
  margin: 0;
  color: var(--pencil);
  font-family: var(--book);
  font-size: 11px;
  line-height: 1.2;
}
.field { margin: 18px 0; }
.field span {
  display: block;
  color: var(--pencil);
  font-size: 15px;
}
.field input {
  width: 100%;
  font-size: 24px;
  line-height: 1.4;
  border-bottom: 1px solid var(--rule);
}
.field input.hand { font-size: 28px; }
.mark {
  display: block;
  width: min(240px, 70vw);
  margin: 18px 0 8px;
  background: var(--paper);
  cursor: pointer;
  text-align: left;
}
.mark svg { display: block; width: 100%; height: auto; }
.mark rect { fill: var(--ink); }
.name-line {
  width: 100%;
  font-size: 34px;
  border-bottom: 1px solid transparent;
}
.public-key {
  display: block;
  width: 100%;
  font-size: 15px;
  line-height: 1.45;
  letter-spacing: 0.02em;
  text-align: left;
  white-space: normal;
  word-break: break-all;
  cursor: pointer;
  user-select: all;
}
"#;

/// Your notes are Kalam. Their notes are Patrick Hand. The page is Iowan Old Style where the
/// system has it, and Libre Baskerville on a phone that does not.
fn paper_css() -> &'static str {
    static CSS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CSS.get_or_init(|| {
        let mut css = String::new();
        css.push_str(&font_face(
            "Kalam",
            include_bytes!("fonts/Kalam-Regular.woff2"),
        ));
        css.push_str(&font_face(
            "Patrick Hand",
            include_bytes!("fonts/PatrickHand-Regular.woff2"),
        ));
        css.push_str(&font_face(
            "Libre Baskerville",
            include_bytes!("fonts/LibreBaskerville-Regular.woff2"),
        ));
        css.push_str(PAPER);
        css
    })
    .as_str()
}

fn font_face(family: &str, bytes: &[u8]) -> String {
    format!(
        "@font-face {{ font-family: \"{family}\"; font-style: normal; font-weight: 400; font-display: swap; src: url(data:font/woff2;base64,{}) format(\"woff2\"); }}\n",
        base64(bytes)
    )
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let value = ((bytes[index] as u32) << 16)
            | ((bytes[index + 1] as u32) << 8)
            | (bytes[index + 2] as u32);
        out.push(TABLE[((value >> 18) & 63) as usize] as char);
        out.push(TABLE[((value >> 12) & 63) as usize] as char);
        out.push(TABLE[((value >> 6) & 63) as usize] as char);
        out.push(TABLE[(value & 63) as usize] as char);
        index += 3;
    }
    match bytes.len() - index {
        1 => {
            let value = (bytes[index] as u32) << 16;
            out.push(TABLE[((value >> 18) & 63) as usize] as char);
            out.push(TABLE[((value >> 12) & 63) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let value = ((bytes[index] as u32) << 16) | ((bytes[index + 1] as u32) << 8);
            out.push(TABLE[((value >> 18) & 63) as usize] as char);
            out.push(TABLE[((value >> 12) & 63) as usize] as char);
            out.push(TABLE[((value >> 6) & 63) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

fn initial_people() -> Vec<Person> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        session::opening_contacts()
    }
    #[cfg(target_arch = "wasm32")]
    {
        sample_people()
    }
}

fn initial_next_id() -> u64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        session::opening_next_id()
    }
    #[cfg(target_arch = "wasm32")]
    {
        4
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn read_if_open(person: &mut Person, page: Page) {
    if let Page::Journal(open) = page {
        if person.id == open {
            mark_read(person);
        }
    }
}

fn commit_people(mut people: Signal<Vec<Person>>, listed: Vec<Person>) {
    #[cfg(not(target_arch = "wasm32"))]
    session::save_contacts(&listed);
    people.set(listed);
}

/// The window iOS has connected, remembered so a window created later can join it.
#[cfg(target_os = "ios")]
static SCENE: std::sync::atomic::AtomicPtr<std::ffi::c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// `UIWindow`s created before the scene connects. iOS 27 only shows a window that belongs to one.
#[cfg(target_os = "ios")]
fn seen_windows() -> &'static std::sync::Mutex<Vec<usize>> {
    static WINDOWS: std::sync::OnceLock<std::sync::Mutex<Vec<usize>>> = std::sync::OnceLock::new();
    WINDOWS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Remembers each window as it gains a root controller, and gives it to the open scene.
#[cfg(target_os = "ios")]
fn watch_for_windows() {
    use std::sync::atomic::{AtomicPtr, Ordering};

    static ORIGINAL: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn class_getInstanceMethod(
            class: *mut std::ffi::c_void,
            selector: *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
        fn method_setImplementation(
            method: *mut std::ffi::c_void,
            implementation: *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
    }

    unsafe extern "C" fn set_root_view_controller(
        window: *mut std::ffi::c_void,
        selector: *mut std::ffi::c_void,
        controller: *mut std::ffi::c_void,
    ) {
        // SAFETY: `ORIGINAL` is the UIKit implementation captured before this wrapper
        // was installed. Calling it performs the real assignment.
        let original: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
        ) = unsafe { std::mem::transmute(ORIGINAL.load(Ordering::Relaxed)) };
        unsafe { original(window, selector, controller) };
        if !seen_windows().lock().unwrap().contains(&(window as usize)) {
            seen_windows().lock().unwrap().push(window as usize);
            eprintln!("touchbase: saw a window");
        }
        attach_known_windows();
    }

    // SAFETY: this runs on the main thread before any window exists. Replacing
    // `setRootViewController:` keeps UIKit's original implementation and records the window.
    unsafe {
        let class = objc_getClass(c"UIWindow".as_ptr());
        let selector = sel_registerName(c"setRootViewController:".as_ptr());
        let method = class_getInstanceMethod(class, selector);
        let previous = method_setImplementation(
            method,
            set_root_view_controller
                as unsafe extern "C" fn(
                    *mut std::ffi::c_void,
                    *mut std::ffi::c_void,
                    *mut std::ffi::c_void,
                ) as *mut std::ffi::c_void,
        );
        ORIGINAL.store(previous, Ordering::Relaxed);
    }
}

/// Gives every window we have seen to the scene iOS connected.
#[cfg(target_os = "ios")]
fn attach_known_windows() {
    use std::sync::atomic::Ordering;

    let scene = SCENE.load(Ordering::Acquire);
    if scene.is_null() {
        return;
    }
    let windows = seen_windows().lock().unwrap().clone();
    for window in windows {
        attach_window(window as *mut std::ffi::c_void, scene);
    }
}

#[cfg(target_os = "ios")]
#[repr(C)]
#[derive(Clone, Copy)]
struct IosPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "ios")]
#[repr(C)]
#[derive(Clone, Copy)]
struct IosSize {
    width: f64,
    height: f64,
}

#[cfg(target_os = "ios")]
#[repr(C)]
#[derive(Clone, Copy)]
struct IosRect {
    origin: IosPoint,
    size: IosSize,
}

/// Puts one window on the connected scene, stretches it to that scene, and shows it.
#[cfg(target_os = "ios")]
fn attach_window(window: *mut std::ffi::c_void, scene: *mut std::ffi::c_void) {
    unsafe extern "C" {
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn objc_msgSend();
    }

    // SAFETY: these pointers come from UIKit on the main thread. `objc_msgSend` uses the
    // C calling convention, including the indirect return used for a rectangle.
    unsafe {
        let send_obj: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void = std::mem::transmute(objc_msgSend as *const ());
        let send_void: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        let send_void_obj: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
        ) = std::mem::transmute(objc_msgSend as *const ());
        let send_rect: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
        ) -> IosRect = std::mem::transmute(objc_msgSend as *const ());
        let send_void_rect: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
            IosRect,
        ) = std::mem::transmute(objc_msgSend as *const ());
        let send_usize: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
        ) -> usize = std::mem::transmute(objc_msgSend as *const ());
        let send_at: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
            usize,
        ) -> *mut std::ffi::c_void = std::mem::transmute(objc_msgSend as *const ());
        let send_mask: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, usize) =
            std::mem::transmute(objc_msgSend as *const ());

        let current = send_obj(window, sel_registerName(c"windowScene".as_ptr()));
        if current != scene {
            send_void_obj(window, sel_registerName(c"setWindowScene:".as_ptr()), scene);
        }

        let space = send_obj(scene, sel_registerName(c"coordinateSpace".as_ptr()));
        let bounds = send_rect(space, sel_registerName(c"bounds".as_ptr()));
        if bounds.size.width < 1.0 || bounds.size.height < 1.0 {
            eprintln!("touchbase: the scene has no size yet");
            return;
        }
        let frame = send_rect(window, sel_registerName(c"frame".as_ptr()));
        eprintln!(
            "touchbase: window {} by {} at {}, {}; scene {} by {}",
            frame.size.width,
            frame.size.height,
            frame.origin.x,
            frame.origin.y,
            bounds.size.width,
            bounds.size.height
        );
        send_void_rect(window, sel_registerName(c"setFrame:".as_ptr()), bounds);

        let root = send_obj(window, sel_registerName(c"rootViewController".as_ptr()));
        let view = send_obj(root, sel_registerName(c"view".as_ptr()));
        if !view.is_null() {
            let view_bounds = IosRect {
                origin: IosPoint { x: 0.0, y: 0.0 },
                size: bounds.size,
            };
            // Flexible width and height, so the page follows the window.
            send_mask(view, sel_registerName(c"setAutoresizingMask:".as_ptr()), 18);
            send_void_rect(view, sel_registerName(c"setFrame:".as_ptr()), view_bounds);
            let subviews = send_obj(view, sel_registerName(c"subviews".as_ptr()));
            let count = send_usize(subviews, sel_registerName(c"count".as_ptr()));
            for index in 0..count {
                let subview = send_at(
                    subviews,
                    sel_registerName(c"objectAtIndex:".as_ptr()),
                    index,
                );
                send_mask(
                    subview,
                    sel_registerName(c"setAutoresizingMask:".as_ptr()),
                    18,
                );
                send_void_rect(
                    subview,
                    sel_registerName(c"setFrame:".as_ptr()),
                    view_bounds,
                );
            }
            send_void(view, sel_registerName(c"layoutIfNeeded".as_ptr()));
        }
        send_void(window, sel_registerName(c"makeKeyAndVisible".as_ptr()));
    }
}

/// Registers the scene delegate named in the app's Info.plist.
///
/// iOS 27 traps at launch unless that manifest names a real scene delegate. The delegate
/// hands the journal window to the scene UIKit creates.
#[cfg(target_os = "ios")]
fn adopt_scene_lifecycle() {
    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn objc_allocateClassPair(
            superclass: *mut std::ffi::c_void,
            name: *const std::ffi::c_char,
            extra_bytes: usize,
        ) -> *mut std::ffi::c_void;
        fn objc_registerClassPair(class: *mut std::ffi::c_void);
        fn class_addMethod(
            class: *mut std::ffi::c_void,
            selector: *mut std::ffi::c_void,
            implementation: *mut std::ffi::c_void,
            types: *const std::ffi::c_char,
        ) -> u8;
        fn class_addProtocol(class: *mut std::ffi::c_void, protocol: *mut std::ffi::c_void) -> u8;
        fn objc_getProtocol(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
    }

    unsafe extern "C" fn scene_will_connect(
        _this: *mut std::ffi::c_void,
        _cmd: *mut std::ffi::c_void,
        scene: *mut std::ffi::c_void,
        _session: *mut std::ffi::c_void,
        _options: *mut std::ffi::c_void,
    ) {
        remember_scene(scene);
    }

    unsafe extern "C" fn scene_did_become_active(
        _this: *mut std::ffi::c_void,
        _cmd: *mut std::ffi::c_void,
        scene: *mut std::ffi::c_void,
    ) {
        remember_scene(scene);
        banner::became_active();
    }

    unsafe extern "C" fn scene_did_enter_background(
        _this: *mut std::ffi::c_void,
        _cmd: *mut std::ffi::c_void,
        _scene: *mut std::ffi::c_void,
    ) {
        banner::schedule_refresh();
    }

    fn remember_scene(scene: *mut std::ffi::c_void) {
        unsafe extern "C" {
            fn objc_getClass(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
            fn sel_registerName(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
            fn objc_msgSend();
        }
        // SAFETY: `scene` is the UIScene UIKit passed into the delegate. The class check
        // uses the UIWindowScene class object.
        let is_window_scene = unsafe {
            let send: unsafe extern "C" fn(
                *mut std::ffi::c_void,
                *mut std::ffi::c_void,
                *mut std::ffi::c_void,
            ) -> u8 = std::mem::transmute(objc_msgSend as *const ());
            send(
                scene,
                sel_registerName(c"isKindOfClass:".as_ptr()),
                objc_getClass(c"UIWindowScene".as_ptr()),
            )
        };
        if is_window_scene == 0 {
            eprintln!("touchbase: the connected scene is not a window scene");
            return;
        }
        SCENE.store(scene, std::sync::atomic::Ordering::Release);
        eprintln!("touchbase: a window scene connected");
        attach_known_windows();
    }

    watch_for_windows();

    // SAFETY: the class is registered before UIKit starts, and the methods match the
    // scene-delegate selectors UIKit calls. The type strings are the Objective-C encodings
    // for those selectors.
    unsafe {
        let superclass = objc_getClass(c"UIResponder".as_ptr());
        let class = objc_allocateClassPair(superclass, c"N10ASceneDelegate".as_ptr(), 0);
        assert!(
            !class.is_null(),
            "the scene delegate class could not be created"
        );
        let added = class_addMethod(
            class,
            sel_registerName(c"scene:willConnectToSession:options:".as_ptr()),
            scene_will_connect
                as unsafe extern "C" fn(
                    *mut std::ffi::c_void,
                    *mut std::ffi::c_void,
                    *mut std::ffi::c_void,
                    *mut std::ffi::c_void,
                    *mut std::ffi::c_void,
                ) as *mut std::ffi::c_void,
            c"v@:@@@".as_ptr(),
        );
        assert!(added != 0, "the scene connection method could not be added");
        let added = class_addMethod(
            class,
            sel_registerName(c"sceneDidBecomeActive:".as_ptr()),
            scene_did_become_active
                as unsafe extern "C" fn(
                    *mut std::ffi::c_void,
                    *mut std::ffi::c_void,
                    *mut std::ffi::c_void,
                ) as *mut std::ffi::c_void,
            c"v@:@".as_ptr(),
        );
        assert!(added != 0, "the scene activation method could not be added");
        let added = class_addMethod(
            class,
            sel_registerName(c"sceneDidEnterBackground:".as_ptr()),
            scene_did_enter_background
                as unsafe extern "C" fn(
                    *mut std::ffi::c_void,
                    *mut std::ffi::c_void,
                    *mut std::ffi::c_void,
                ) as *mut std::ffi::c_void,
            c"v@:@".as_ptr(),
        );
        assert!(added != 0, "the scene background method could not be added");
        for protocol_name in [c"UISceneDelegate", c"UIWindowSceneDelegate"] {
            let protocol = objc_getProtocol(protocol_name.as_ptr());
            assert!(
                !protocol.is_null(),
                "a scene protocol is missing from UIKit"
            );
            let added = class_addProtocol(class, protocol);
            assert!(added != 0, "the scene protocol could not be adopted");
        }
        objc_registerClassPair(class);
    }
}

/// Prints the name and reason of an Objective-C exception before it crosses into Rust.
#[cfg(target_os = "ios")]
fn note_objc_exceptions() {
    unsafe extern "C" {
        fn objc_setExceptionPreprocessor(
            callback: unsafe extern "C" fn(*mut std::ffi::c_void) -> *mut std::ffi::c_void,
        );
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn objc_msgSend();
    }

    unsafe extern "C" fn log_exception(exception: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
        // SAFETY: `exception` is the NSException Apple is about to throw. `name` and
        // `reason` return NSStrings, and `UTF8String` is valid for this call.
        unsafe {
            let send: unsafe extern "C" fn(
                *mut std::ffi::c_void,
                *mut std::ffi::c_void,
            ) -> *mut std::ffi::c_void = std::mem::transmute(objc_msgSend as *const ());
            let name = sel_registerName(c"name".as_ptr());
            let reason = sel_registerName(c"reason".as_ptr());
            let utf8 = sel_registerName(c"UTF8String".as_ptr());
            let name = send(send(exception, name), utf8);
            let reason = send(send(exception, reason), utf8);
            let name = if name.is_null() {
                c"(none)".as_ptr()
            } else {
                name.cast()
            };
            let reason = if reason.is_null() {
                c"(none)".as_ptr()
            } else {
                reason.cast()
            };
            eprintln!(
                "touchbase: objc exception: {} — {}",
                std::ffi::CStr::from_ptr(name).to_string_lossy(),
                std::ffi::CStr::from_ptr(reason).to_string_lossy()
            );
        }
        exception
    }

    // SAFETY: the preprocessor must return the same exception object. It only reads
    // the exception's name and reason before Apple throws it.
    unsafe {
        objc_setExceptionPreprocessor(log_exception);
    }
}

fn main() {
    #[cfg(feature = "desktop")]
    {
        use dioxus::desktop::{Config, WindowBuilder};
        session::start_default().expect("the journal session could not start");
        dioxus::LaunchBuilder::desktop()
            .with_cfg(
                Config::new().with_window(
                    WindowBuilder::new()
                        .with_title("N10A")
                        .with_inner_size(dioxus::desktop::LogicalSize::new(440.0, 760.0)),
                ),
            )
            .launch(App);
    }
    #[cfg(all(feature = "mobile", not(feature = "desktop")))]
    {
        #[cfg(target_os = "ios")]
        {
            note_objc_exceptions();
            banner::install();
            adopt_scene_lifecycle();
        }
        eprintln!("touchbase: starting the journal");
        session::start_default().expect("the journal session could not start");
        eprintln!("touchbase: opening the window");
        dioxus::LaunchBuilder::mobile().launch(App);
    }
    #[cfg(all(feature = "web", not(feature = "desktop"), not(feature = "mobile")))]
    dioxus::launch(App);
}

#[derive(Clone, PartialEq, Eq)]
struct VoiceLine {
    identity: String,
    kind: u8,
    words: String,
    ticket: u64,
    video: bool,
}

impl VoiceLine {
    fn quiet() -> Self {
        Self {
            identity: String::new(),
            kind: 0,
            words: String::new(),
            ticket: 0,
            video: false,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn voice_line() -> VoiceLine {
    match session::call_face() {
        session::CallFace::Quiet => VoiceLine::quiet(),
        session::CallFace::Placing { identity, outgoing } => VoiceLine {
            identity,
            kind: 1,
            words: if outgoing {
                "Calling…".to_string()
            } else {
                "Answering…".to_string()
            },
            ticket: 0,
            video: session::call_is_video(),
        },
        session::CallFace::Live { identity } => {
            let video = session::call_is_video();
            VoiceLine {
                identity,
                kind: 2,
                words: if video {
                    "On video".to_string()
                } else {
                    "On a call".to_string()
                },
                ticket: 0,
                video,
            }
        }
        session::CallFace::Failed { identity, words } => VoiceLine {
            identity,
            kind: 3,
            words: words.to_string(),
            ticket: 0,
            video: false,
        },
        session::CallFace::Ringing { identity, ticket } => VoiceLine {
            identity,
            kind: 4,
            words: String::new(),
            ticket,
            video: false,
        },
    }
}

fn shown_picture(show: bool) -> Option<String> {
    if !show {
        return None;
    }
    #[cfg(any(target_os = "macos", target_os = "android"))]
    {
        return session::video_picture();
    }
    #[cfg(not(any(target_os = "macos", target_os = "android")))]
    {
        None
    }
}

fn this_call(person: &str, voice: &VoiceLine) -> bool {
    if voice.kind == 0 || voice.identity.is_empty() {
        return false;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        session::same_contact(person, &voice.identity)
    }
    #[cfg(target_arch = "wasm32")]
    {
        person.eq_ignore_ascii_case(&voice.identity)
    }
}

#[component]
fn IncomingBar(voice: VoiceLine, people: Vec<Person>) -> Element {
    if voice.kind != 4 {
        return rsx! {};
    }
    let name = people
        .into_iter()
        .find(|person| this_call(&person.identity, &voice))
        .map(|person| person.name)
        .unwrap_or_else(|| "Someone".to_string());
    let ticket = voice.ticket;
    rsx! {
        div { class: "incoming",
            p { class: "hand", "{name} is calling" }
            button {
                onclick: move |_| {
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        chime::ring(false);
                        session::answer(ticket);
                    }
                },
                "answer"
            }
            button {
                onclick: move |_| {
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        chime::ring(false);
                        session::decline(ticket);
                    }
                },
                "decline"
            }
        }
    }
}

#[cfg(feature = "desktop")]
fn watch_focus() {
    use dioxus::desktop::tao::event::{Event, WindowEvent};
    use dioxus::desktop::use_wry_event_handler;
    use_wry_event_handler(|event, _| {
        if let Event::WindowEvent {
            event: WindowEvent::Focused(focused),
            ..
        } = event
        {
            banner::set_in_front(*focused);
        }
    });
}

#[allow(non_snake_case)]
fn App() -> Element {
    #[cfg(feature = "desktop")]
    watch_focus();
    let mut page = use_signal(|| Page::People);
    let mut cover = use_signal(|| Page::People);
    let mut your_name = use_signal(machine_name);
    let people = use_signal(initial_people);
    let mut next_id = use_signal(initial_next_id);
    let mut draft_name = use_signal(String::new);
    let mut draft_code = use_signal(String::new);
    let mut draft_note = use_signal(String::new);
    let mut draft_place = use_signal(|| None::<Place>);
    let mut place_epoch = use_signal(|| 0_u64);
    let mut visit = use_signal(|| 0_u64);
    let mut wires = use_signal(read_wires);
    let mut voice = use_signal(VoiceLine::quiet);
    let mut film_at = use_signal(|| 0u32);
    let mut reach = use_signal(Reach::unknown);
    let mut tail = use_signal(|| 0_u64);
    let mut transfer = use_signal(TransferFace::default);
    let download_toast = use_signal(|| (0_u64, 0_u64));

    #[cfg(not(target_arch = "wasm32"))]
    {
        static LISTENING: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if LISTENING.set(()).is_ok() {
            spawn(async move {
                loop {
                    let next = read_wires();
                    if wires() != next {
                        wires.set(next);
                    }
                    for arrived in session::drain_inbox() {
                        let mut listed = people();
                        if let Some(person) = listed.iter_mut().find(|person| {
                            session::same_contact(&person.identity, &arrived.identity)
                        }) {
                            if arrived.from_mailbox {
                                session::send_ack(&person.identity, &arrived.digest);
                            }
                            let named = if !arrived.file_name.is_empty() {
                                Some(arrived.file_name.as_str())
                            } else if !arrived.file_bytes.is_empty() {
                                Some("file")
                            } else {
                                None
                            };
                            if note_from_them(
                                person,
                                &arrived.words,
                                arrived.written_at,
                                arrived.place,
                                named,
                            ) {
                                if !arrived.file_bytes.is_empty() {
                                    if let Some(id) = person.notes.last().map(|note| note.id) {
                                        session::keep_file(id, &arrived.file_bytes);
                                        let saved_name = if arrived.file_name.is_empty() {
                                            "file"
                                        } else {
                                            arrived.file_name.as_str()
                                        };
                                        if session::publish_download(id, saved_name)
                                            && land_their_file(person, id)
                                        {
                                            show_download_toast(download_toast, id);
                                        }
                                        session::trace(&format!(
                                            "kept {} bytes as {saved_name}",
                                            arrived.file_bytes.len()
                                        ));
                                    }
                                }
                                let name = person.name.clone();
                                let words = if arrived.words.trim().is_empty() {
                                    if arrived.file_name.is_empty() {
                                        "file".to_string()
                                    } else {
                                        arrived.file_name.clone()
                                    }
                                } else {
                                    shown_words(&arrived.words, arrived.place)
                                };
                                read_if_open(person, page());
                                commit_people(people, listed);
                                if banner::in_front() {
                                    chime::arrived();
                                } else {
                                    banner::note(&name, &words);
                                }
                            } else if !arrived.file_bytes.is_empty() {
                                let saved_name = if arrived.file_name.is_empty() {
                                    "file"
                                } else {
                                    arrived.file_name.as_str()
                                };
                                if let Some(note) = person.notes.iter_mut().rev().find(|note| {
                                    !note.mine && note.file_name.as_deref() == Some(saved_name)
                                }) {
                                    let id = note.id;
                                    let fresh = !note.downloaded;
                                    note.failed = false;
                                    session::keep_file(id, &arrived.file_bytes);
                                    if session::publish_download(id, saved_name) {
                                        note.downloaded = true;
                                        if fresh {
                                            show_download_toast(download_toast, id);
                                        }
                                    }
                                    session::trace(&format!(
                                        "kept {} bytes as {saved_name}",
                                        arrived.file_bytes.len()
                                    ));
                                    commit_people(people, listed);
                                } else {
                                    session::trace(&format!(
                                        "did not keep {} bytes ({saved_name})",
                                        arrived.file_bytes.len()
                                    ));
                                }
                            } else if !arrived.file_name.is_empty() {
                                let saved_name = arrived.file_name.as_str();
                                if let Some(note) = person.notes.iter_mut().rev().find(|note| {
                                    !note.mine
                                        && note.written_at == arrived.written_at.to_bits()
                                        && note.file_name.as_deref() == Some(saved_name)
                                        && note.failed
                                }) {
                                    note.failed = false;
                                    commit_people(people, listed);
                                }
                            }
                        } else if !arrived.file_bytes.is_empty() {
                            session::trace(&format!(
                                "arrived file {} bytes with no matching contact",
                                arrived.file_bytes.len()
                            ));
                        }
                    }
                    for arrived in session::drain_reactions() {
                        let mut listed = people();
                        if let Some(person) = listed.iter_mut().find(|person| {
                            session::same_contact(&person.identity, &arrived.identity)
                        }) {
                            if apply_reaction(
                                person,
                                arrived.written_at,
                                &arrived.words,
                                arrived.reaction,
                            ) {
                                commit_people(people, listed);
                                chime::arrived();
                            }
                        }
                    }
                    for invite in session::drain_invites() {
                        let known = people().iter().any(|person| {
                            session::same_contact(&person.identity, &invite.identity)
                        });
                        if known {
                            session::offer_incoming(invite);
                        } else {
                            session::decline(invite.ticket);
                        }
                    }
                    let posted = session::drain_posted();
                    if !posted.is_empty() {
                        let mut listed = people();
                        let mut changed = false;
                        for (id, digest) in posted {
                            changed |= post_note(&mut listed, id, digest);
                        }
                        if changed {
                            commit_people(people, listed);
                        }
                    }
                    let acks = session::drain_acks();
                    if !acks.is_empty() {
                        let mut listed = people();
                        let mut changed = false;
                        for id in acks {
                            changed |= settle_note(&mut listed, id);
                        }
                        if changed {
                            commit_people(people, listed);
                        }
                    }
                    let failed = session::drain_failures();
                    if !failed.is_empty() {
                        let mut listed = people();
                        let mut changed = false;
                        for id in failed {
                            changed |= fail_note(&mut listed, id);
                        }
                        if changed {
                            commit_people(people, listed);
                        }
                    }
                    let missed = session::drain_incoming_failures();
                    if !missed.is_empty() {
                        let mut listed = people();
                        let mut changed = false;
                        for missed in missed {
                            let Some(person) = listed.iter_mut().find(|person| {
                                session::same_contact(&person.identity, &missed.identity)
                            }) else {
                                continue;
                            };
                            if let Some(id) = person.notes.iter().rev().find_map(|note| {
                                (!note.mine
                                    && note.written_at == missed.written_at.to_bits()
                                    && note.file_name.as_deref() == Some(missed.file_name.as_str()))
                                .then_some(note.id)
                            }) {
                                if session::kept_file(id).is_some() {
                                    continue;
                                }
                            }
                            changed |=
                                fail_their_file(person, missed.written_at, &missed.file_name);
                        }
                        if changed {
                            commit_people(people, listed);
                        }
                    }
                    if let Some(done) = session::take_finished() {
                        let mut listed = people();
                        if let Some(person) = listed
                            .iter_mut()
                            .find(|person| session::same_contact(&person.identity, &done.identity))
                        {
                            remember_call(person, done.seconds, done.outgoing);
                            read_if_open(person, page());
                            commit_people(people, listed);
                        }
                    }
                    let next = voice_line();
                    chime::ring(next.kind == 4);
                    voice::ringback(session::caller_is_waiting());
                    // The call line stays "On video" while new frames arrive, so the
                    // picture has its own tick. Without it the first look happens
                    // before a frame exists and the window never shows one.
                    #[cfg(any(target_os = "macos", target_os = "android"))]
                    let fresh = next.video
                        && (next.kind == 1 || next.kind == 2)
                        && session::video_picture().is_some();
                    if voice() != next {
                        voice.set(next);
                    }
                    #[cfg(any(target_os = "macos", target_os = "android"))]
                    if fresh {
                        film_at.set(film_at().wrapping_add(1));
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    if let Page::Journal(id) = page() {
                        if let Some(person) = people().iter().find(|person| person.id == id) {
                            session::measure(&person.identity);
                            session::watch(&person.identity);
                            let next = session::reach_for(&person.identity);
                            if reach() != next {
                                reach.set(next);
                            }
                        }
                    }
                    let next = TransferFace::from_session(session::transfer_view().await);
                    if transfer() != next {
                        transfer.set(next);
                    }
                    session::pause().await;
                }
            });
        }
    }

    let open = !matches!(page(), Page::People);

    rsx! {
        style { {paper_css()} }
        div { class: "notebook",
            onmounted: move |_| {
                document::eval(KEEP_KEYBOARD);
                #[cfg(target_os = "macos")]
                banner::became_active();
            },
            WireLetters { wires: wires() }
            IncomingBar { voice: voice(), people: people() }
            PeoplePage {
                people: people(),
                on_open: move |id| {
                    draft_note.set(String::new());
                    visit.set(visit() + 1);
                    reach.set(Reach::unknown());
                    let mut listed = people();
                    let identity = listed
                        .iter()
                        .find(|person| person.id == id)
                        .map(|person| person.identity.clone());
                    if let Some(person) = listed.iter_mut().find(|person| person.id == id) {
                        if mark_read(person) {
                            commit_people(people, listed);
                        }
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    if let Some(identity) = identity {
                        session::forget(&identity);
                    }
                    let next = Page::Journal(id);
                    cover.set(next);
                    page.set(next);
                },
                on_code: move |_| {
                    cover.set(Page::Code);
                    page.set(Page::Code);
                },
                on_add: move |_| {
                    draft_name.set(String::new());
                    draft_code.set(String::new());
                    cover.set(Page::Add);
                    page.set(Page::Add);
                },
                on_rename: move |(id, name): (u64, String)| {
                    let mut listed = people();
                    if rename_person(&mut listed, id, &name) {
                        commit_people(people, listed);
                    }
                },
                on_remove: move |id| {
                    let mut listed = people();
                    let identity = listed
                        .iter()
                        .find(|person| person.id == id)
                        .map(|person| person.identity.clone());
                    if !remove_person(&mut listed, id) {
                        return;
                    }
                    let identity = identity.expect("the removed contact");
                    commit_people(people, listed);
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        session::forget(&identity);
                        let line = voice();
                        if this_call(&identity, &line) {
                            if line.kind == 4 {
                                session::decline(line.ticket);
                            } else {
                                session::hang_up();
                            }
                        }
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        let _ = identity;
                    }
                    if let Page::Journal(open) = page() {
                        if open == id {
                            cover.set(Page::People);
                            page.set(Page::People);
                        }
                    }
                },
            }
            div { class: if open { "sheet open" } else { "sheet" },
                match cover() {
                    Page::People => rsx! {},
                    Page::Code => rsx! {
                        CodePage {
                            name: your_name(),
                            public_key: your_public_key(),
                            on_name: move |name| your_name.set(name),
                            on_back: move |_| page.set(Page::People),
                        }
                    },
                    Page::Add => rsx! {
                        AddPage {
                            name: draft_name(),
                            code: draft_code(),
                            on_name: move |value| draft_name.set(value),
                            on_code: move |value| draft_code.set(value),
                            on_back: move |_| page.set(Page::People),
                            on_scanned: move |(name, identity): (String, String)| {
                                let mut listed = people();
                                let mut id = next_id();
                                if add_person(&mut listed, &mut id, &name, &identity) {
                                    commit_people(people, listed);
                                    next_id.set(id);
                                    page.set(Page::People);
                                }
                            },
                            on_save: move |_| {
                                let mut listed = people();
                                let mut id = next_id();
                                if add_person(&mut listed, &mut id, &draft_name(), &draft_code()) {
                                    commit_people(people, listed);
                                    next_id.set(id);
                                    page.set(Page::People);
                                }
                            },
                        }
                    },
                    Page::Journal(id) => {
                        let person = people().into_iter().find(|person| person.id == id);
                        match person {
                            Some(person) => rsx! {
                                JournalPage {
                                    key: "{person.id}-{visit()}",
                                    person: person.clone(),
                                    draft: draft_note(),
                                    voice: voice(),
                                    film_at: film_at(),
                                    reach: reach(),
                                    place_epoch: place_epoch,
                                    tail: tail(),
                                    transfer: transfer(),
                                    landed: download_toast().0,
                                    on_draft: move |value| draft_note.set(value),
                                    on_place: move |place| {
                                        draft_place.set(Some(place));
                                        let mut text = draft_note();
                                        if !text.contains(PLACEHOLDER) {
                                            if !text.is_empty()
                                                && !text.ends_with(' ')
                                                && !text.ends_with('\n')
                                            {
                                                text.push(' ');
                                            }
                                            text.push_str(PLACEHOLDER);
                                            draft_note.set(text);
                                        }
                                    },
                                    on_back: move |_| page.set(Page::People),
                                    on_retry: move |note_id: u64| {
                                        let mut listed = people();
                                        let Some(person) =
                                            listed.iter_mut().find(|person| person.id == id)
                                        else {
                                            return;
                                        };
                                        #[cfg(not(target_arch = "wasm32"))]
                                        let stored = session::kept_file(note_id);
                                        #[cfg(target_arch = "wasm32")]
                                        let stored: Option<Vec<u8>> = None;
                                        let file_only_gone = stored.is_none()
                                            && person.notes.iter().any(|note| {
                                                note.id == note_id
                                                    && note.words.trim().is_empty()
                                                    && note.file_name.is_some()
                                            });
                                        if file_only_gone {
                                            return;
                                        }
                                        let Some((words, written_at, place, file_name)) =
                                            retry_note(person, note_id)
                                        else {
                                            return;
                                        };
                                        let identity = person.identity.clone();
                                        commit_people(people, listed);
                                        #[cfg(not(target_arch = "wasm32"))]
                                        {
                                            let file = match (&file_name, &stored) {
                                                (Some(name), Some(bytes)) if !bytes.is_empty() => {
                                                    Some((name.as_str(), bytes.as_slice()))
                                                }
                                                _ => None,
                                            };
                                            session::send_note(
                                                &identity,
                                                &words,
                                                note_id,
                                                written_at,
                                                place,
                                                file,
                                            );
                                        }
                                        #[cfg(target_arch = "wasm32")]
                                        {
                                            let _ = (
                                                identity, words, written_at, place, note_id,
                                                file_name, stored,
                                            );
                                        }
                                    },
                                    on_react: move |(note_id, choice): (u64, Reaction)| {
                                        let mut listed = people();
                                        let Some(person) =
                                            listed.iter_mut().find(|person| person.id == id)
                                        else {
                                            return;
                                        };
                                        let Some(bits) = react_to_note(person, note_id, choice) else {
                                            return;
                                        };
                                        let identity = person.identity.clone();
                                        let found = person.notes.iter().find(|note| note.id == note_id);
                                        let words = found
                                            .map(|note| note.words.clone())
                                            .unwrap_or_default();
                                        let reaction = found.and_then(|note| note.reaction);
                                        commit_people(people, listed);
                                        #[cfg(not(target_arch = "wasm32"))]
                                        session::send_reaction(
                                            &identity,
                                            f64::from_bits(bits),
                                            &words,
                                            reaction,
                                        );
                                        #[cfg(target_arch = "wasm32")]
                                        {
                                            let _ = (bits, identity, words, reaction);
                                        }
                                    },
                                    on_write: move |file: Option<(String, Vec<u8>)>| {
                                        let words = draft_note();
                                        let place = if words.contains(PLACEHOLDER) {
                                            draft_place()
                                        } else {
                                            None
                                        };
                                        let mut listed = people();
                                        if let Some(person) = listed.iter_mut().find(|person| person.id == id) {
                                            let note_id = if let Some((name, _)) = &file {
                                                write_file_note(person, &words, place, name)
                                            } else {
                                                match place {
                                                    Some(place) => {
                                                        write_placed_note(person, &words, Some(place))
                                                    }
                                                    None => write_note(person, &words),
                                                }
                                            };
                                            #[cfg(not(target_arch = "wasm32"))]
                                            if let Some(note_id) = note_id {
                                                if let Some((_, bytes)) = &file {
                                                    session::keep_file(note_id, bytes);
                                                }
                                                let written_at = person
                                                    .notes
                                                    .iter()
                                                    .find(|note| note.id == note_id)
                                                    .map(|note| f64::from_bits(note.written_at))
                                                    .unwrap_or(0.0);
                                                let sending = file.as_ref().map(|(name, bytes)| {
                                                    (name.as_str(), bytes.as_slice())
                                                });
                                                session::send_note(
                                                    &person.identity,
                                                    &words,
                                                    note_id,
                                                    written_at,
                                                    place,
                                                    sending,
                                                );
                                            }
                                            #[cfg(target_arch = "wasm32")]
                                            let _ = (note_id, &file);
                                            tail.set(tail() + 1);
                                        }
                                        commit_people(people, listed);
                                        draft_note.set(String::new());
                                        draft_place.set(None);
                                        place_epoch.set(place_epoch() + 1);
                                    },
                                }
                            },
                            None => rsx! {},
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Wires {
    wifi: bool,
    internet: bool,
    ble: bool,
    usb: bool,
}

fn read_wires() -> Wires {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let marks = session::wire_marks();
        return Wires {
            wifi: marks.wifi,
            internet: marks.internet,
            ble: marks.ble,
            usb: marks.usb,
        };
    }
    #[cfg(target_arch = "wasm32")]
    Wires {
        wifi: false,
        internet: false,
        ble: false,
        usb: false,
    }
}

#[component]
fn WireLetters(wires: Wires) -> Element {
    rsx! {
        div { class: "wires",
            span { class: if wires.wifi { "live" } else { "quiet" }, title: "Wifi auto", "W" }
            span { class: if wires.internet { "live" } else { "quiet" }, title: "Internet", "I" }
            span { class: if wires.ble { "live" } else { "quiet" }, title: "BLE", "B" }
            span { class: if wires.usb { "live" } else { "quiet" }, title: "USB", "U" }
        }
    }
}

const SEAM: f64 = 56.0;

fn people_page_class(live: bool) -> &'static str {
    match (cfg!(target_os = "android"), live) {
        (true, true) => "page people pulling android",
        (true, false) => "page people android",
        (false, true) => "page people pulling",
        (false, false) => "page people",
    }
}

fn close_seam(
    mut pull: Signal<Option<(f64, f64)>>,
    mut pulling: Signal<bool>,
    mut wheeling: Signal<bool>,
    mut wheel_mark: Signal<u64>,
    mut seam: Signal<f64>,
) {
    pull.set(None);
    pulling.set(false);
    wheeling.set(false);
    wheel_mark.set(wheel_mark() + 1);
    seam.set(0.0);
}

fn release_seam(
    pull: Signal<Option<(f64, f64)>>,
    pulling: Signal<bool>,
    wheeling: Signal<bool>,
    wheel_mark: Signal<u64>,
    seam: Signal<f64>,
    mut pulled: Signal<bool>,
) {
    let opened = pulling();
    close_seam(pull, pulling, wheeling, wheel_mark, seam);
    if !opened {
        return;
    }
    // The click that follows a pull would otherwise open Add or the QR page.
    spawn(async move {
        let mut eval = document::eval("setTimeout(function () { dioxus.send(true); }, 0);");
        let _ = eval.recv::<bool>().await;
        pulled.set(false);
    });
}

fn wheel_pull(delta: dioxus::html::geometry::WheelDelta) -> f64 {
    match delta {
        dioxus::html::geometry::WheelDelta::Pixels(v) => v.y,
        dioxus::html::geometry::WheelDelta::Lines(v) => v.y * 28.0,
        dioxus::html::geometry::WheelDelta::Pages(v) => v.y * SEAM,
    }
}

fn app_version() -> String {
    env!("TOUCHBASE_VERSION").to_string()
}

#[component]
fn PeoplePage(
    people: Vec<Person>,
    on_open: EventHandler<u64>,
    on_code: EventHandler<()>,
    on_add: EventHandler<()>,
    on_rename: EventHandler<(u64, String)>,
    on_remove: EventHandler<u64>,
) -> Element {
    let mut confirming = use_signal(|| None::<u64>);
    let mut renaming = use_signal(|| None::<u64>);
    let mut draft = use_signal(String::new);
    let mut at_top = use_signal(|| true);
    let mut seam = use_signal(|| 0.0);
    let mut pulling = use_signal(|| false);
    let mut pull = use_signal(|| None::<(f64, f64)>);
    let mut pulled = use_signal(|| false);
    let mut wheeling = use_signal(|| false);
    let mut wheel_mark = use_signal(|| 0u64);
    let version = app_version();
    let motion = if pulling() || wheeling() {
        "none"
    } else {
        "180ms ease"
    };
    let people_class = people_page_class(pulling() || wheeling());
    rsx! {
        style { ":root {{ --seam: {seam()}px; --seam-move: {motion}; }}" }
        div {
            class: "{people_class}",
            onmounted: move |_| {
                #[cfg(target_os = "android")]
                document::eval(SEAM_TOUCH);
            },
            onscroll: move |event| {
                let top = event.data().scroll_top() <= 0.5;
                at_top.set(top);
                if !top {
                    close_seam(pull, pulling, wheeling, wheel_mark, seam);
                }
            },
            onpointerdown: move |event| {
                wheel_mark.set(wheel_mark() + 1);
                wheeling.set(false);
                if !at_top() {
                    return;
                }
                let point = event.client_coordinates();
                pull.set(Some((point.x, point.y)));
                pulling.set(false);
            },
            onpointermove: move |event| {
                let Some((start_x, start_y)) = pull() else {
                    return;
                };
                let point = event.client_coordinates();
                let dx = point.x - start_x;
                let dy = point.y - start_y;
                if !pulling() {
                    if dx.abs() < 18.0 && dy < 18.0 {
                        return;
                    }
                    // A sideways slide still belongs to the contact row.
                    if dy <= dx.abs() {
                        pull.set(None);
                        return;
                    }
                    pulling.set(true);
                    pulled.set(true);
                }
                if !at_top() {
                    close_seam(pull, pulling, wheeling, wheel_mark, seam);
                    return;
                }
                event.prevent_default();
                seam.set(dy.clamp(0.0, SEAM));
            },
            onpointerup: move |_| release_seam(pull, pulling, wheeling, wheel_mark, seam, pulled),
            onpointercancel: move |_| {
                release_seam(pull, pulling, wheeling, wheel_mark, seam, pulled);
            },
            onpointerleave: move |_| {
                if pulling() {
                    release_seam(pull, pulling, wheeling, wheel_mark, seam, pulled);
                } else {
                    pull.set(None);
                }
            },
            onwheel: move |event| {
                if pulling() {
                    return;
                }
                let dy = wheel_pull(event.delta());
                if dy >= 0.0 {
                    if seam() > 0.0 {
                        event.prevent_default();
                        let next = (seam() - dy).max(0.0);
                        seam.set(next);
                        if next == 0.0 {
                            wheeling.set(false);
                        }
                    }
                    return;
                }
                if !at_top() {
                    return;
                }
                event.prevent_default();
                wheeling.set(true);
                seam.set((seam() - dy).min(SEAM));
                let mark = wheel_mark() + 1;
                wheel_mark.set(mark);
                spawn(async move {
                    let mut eval = document::eval(
                        "setTimeout(function () { dioxus.send(true); }, 180);",
                    );
                    let _ = eval.recv::<bool>().await;
                    if wheel_mark() == mark {
                        wheeling.set(false);
                        seam.set(0.0);
                    }
                });
            },
            div { class: "seam",
                span { "{version}" }
            }
            div { class: "people-body",
            div { class: "actions",
                button {
                    class: "action",
                    onclick: move |_| {
                        if pulled() {
                            pulled.set(false);
                            return;
                        }
                        on_add.call(());
                    },
                    "Add a contact"
                }
                button {
                    class: "action",
                    onclick: move |_| {
                        if pulled() {
                            pulled.set(false);
                            return;
                        }
                        on_code.call(());
                    },
                    "Your QR code"
                }
            }
            if people.is_empty() {
                p { class: "empty-book", "Add a contact by scanning their QR code" }
            } else {
            ul { class: "names",
                for person in people {
                    PersonRow {
                        key: "{person.id}",
                        id: person.id,
                        name: person.name.clone(),
                        unread: has_unread(&person),
                        confirming: confirming() == Some(person.id),
                        editing: renaming() == Some(person.id),
                        draft: draft(),
                        on_open: move |id| {
                            confirming.set(None);
                            renaming.set(None);
                            on_open.call(id);
                        },
                        on_ask: move |id| {
                            renaming.set(None);
                            confirming.set(Some(id));
                        },
                        on_keep: move |_| confirming.set(None),
                        on_remove: move |id| {
                            confirming.set(None);
                            on_remove.call(id);
                        },
                        on_edit: {
                            let current = person.name.clone();
                            move |id| {
                                confirming.set(None);
                                draft.set(current.clone());
                                renaming.set(Some(id));
                            }
                        },
                        on_draft: move |value| draft.set(value),
                        on_named: move |(id, name)| {
                            renaming.set(None);
                            on_rename.call((id, name));
                        },
                        on_cancel: move |_| renaming.set(None),
                    }
                }
            }
            }
            }
        }
    }
}

const ACTION: f64 = 84.0;

#[component]
fn PersonRow(
    id: u64,
    name: String,
    unread: bool,
    confirming: bool,
    editing: bool,
    draft: String,
    on_open: EventHandler<u64>,
    on_ask: EventHandler<u64>,
    on_keep: EventHandler<()>,
    on_remove: EventHandler<u64>,
    on_edit: EventHandler<u64>,
    on_draft: EventHandler<String>,
    on_named: EventHandler<(u64, String)>,
    on_cancel: EventHandler<()>,
) -> Element {
    let mut reveal = use_signal(|| 0.0);
    let mut drag = use_signal(|| None::<(f64, f64, f64)>);
    let mut dragged = use_signal(|| false);
    let mut dismissed = use_signal(|| false);
    let ask = name.clone();
    if editing {
        let on_key = draft.clone();
        let on_leave = draft.clone();
        return rsx! {
            li {
                input {
                    class: "who rename",
                    aria_label: "Name",
                    value: "{draft}",
                    onmounted: move |event| {
                        spawn(async move {
                            let _ = event.set_focus(true).await;
                        });
                    },
                    oninput: move |event| on_draft.call(event.value()),
                    onkeydown: move |event| {
                        if event.key() == Key::Escape {
                            dismissed.set(true);
                            on_cancel.call(());
                        } else if event.key() == Key::Enter {
                            event.prevent_default();
                            dismissed.set(true);
                            finish_rename(id, &on_key, on_named, on_cancel);
                        }
                    },
                    onblur: move |_| {
                        if dismissed() {
                            dismissed.set(false);
                            return;
                        }
                        finish_rename(id, &on_leave, on_named, on_cancel);
                    },
                }
            }
        };
    }
    if confirming {
        return rsx! {
            li {
                div { class: "confirm-remove",
                    p { "Remove {ask}? All history will be deleted." }
                    div { class: "confirm-actions",
                        button { onclick: move |_| on_remove.call(id), "Remove" }
                        button { onclick: move |_| on_keep.call(()), "Keep" }
                    }
                }
            }
        };
    }
    let config = if reveal() < 0.0 { -reveal() } else { 0.0 };
    let remove = if reveal() > 0.0 { reveal() } else { 0.0 };
    rsx! {
        li {
            div {
                class: if drag().is_some() { "name-swipe live" } else { "name-swipe" },
                style: "--config: {config}px; --remove: {remove}px",
                onpointerdown: move |event| {
                    let point = event.client_coordinates();
                    drag.set(Some((point.x, point.y, reveal())));
                    dragged.set(false);
                },
                onpointermove: move |event| {
                    let Some((start_x, start_y, start_reveal)) = drag() else {
                        return;
                    };
                    let point = event.client_coordinates();
                    let travel = point.x - start_x;
                    let dy = point.y - start_y;
                    if travel.abs() < 8.0 && dy.abs() > travel.abs() {
                        return;
                    }
                    // A pull to the right opens the actions on the left. A pull to the left opens removal.
                    reveal.set((start_reveal - travel).clamp(-ACTION, ACTION));
                },
                onpointerup: move |event| {
                    let tap = drag().is_some_and(|(start_x, start_y, _)| {
                        let point = event.client_coordinates();
                        let dx = point.x - start_x;
                        let dy = point.y - start_y;
                        dx * dx + dy * dy < 18.0 * 18.0
                    });
                    release_row(drag, reveal);
                    dragged.set(!tap);
                },
                onpointercancel: move |_| release_row(drag, reveal),
                onpointerleave: move |_| release_row(drag, reveal),
                div { class: "name-config",
                    button {
                        aria_label: "Rename",
                        onpointerdown: move |event| event.stop_propagation(),
                        onpointerup: move |event| event.stop_propagation(),
                        onclick: move |event| {
                            event.stop_propagation();
                            reveal.set(0.0);
                            on_edit.call(id);
                        },
                        "rename"
                    }
                }
                button {
                    class: "who",
                    onclick: move |_| {
                        if dragged() {
                            dragged.set(false);
                            return;
                        }
                        on_open.call(id);
                    },
                    if unread {
                        span { class: "fresh", "*" }
                    }
                    "{name}"
                }
                div { class: "name-remove",
                    button {
                        aria_label: "Remove",
                        onpointerdown: move |event| event.stop_propagation(),
                        onpointerup: move |event| event.stop_propagation(),
                        onclick: move |event| {
                            event.stop_propagation();
                            reveal.set(0.0);
                            on_ask.call(id);
                        },
                        "remove"
                    }
                }
            }
        }
    }
}

fn release_row(mut drag: Signal<Option<(f64, f64, f64)>>, mut reveal: Signal<f64>) {
    if drag().is_none() {
        return;
    }
    drag.set(None);
    let value = reveal();
    let next = if value > ACTION / 2.0 {
        ACTION
    } else if value < -ACTION / 2.0 {
        -ACTION
    } else {
        0.0
    };
    reveal.set(next);
}

fn finish_rename(
    id: u64,
    draft: &str,
    on_named: EventHandler<(u64, String)>,
    on_cancel: EventHandler<()>,
) {
    let name = draft.trim();
    if name.is_empty() {
        on_cancel.call(());
    } else {
        on_named.call((id, name.to_string()));
    }
}

fn machine_name() -> String {
    #[cfg(target_os = "macos")]
    {
        return "Mac".to_string();
    }
    #[cfg(target_os = "ios")]
    {
        return "iPhone".to_string();
    }
    #[cfg(target_os = "android")]
    {
        return session::machine_name();
    }
    #[cfg(target_os = "windows")]
    {
        return "PC".to_string();
    }
    #[cfg(target_arch = "wasm32")]
    {
        return "Browser".to_string();
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "android",
        target_os = "windows",
        target_arch = "wasm32",
    )))]
    {
        "Computer".to_string()
    }
}

#[component]
fn your_public_key() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(key) = session::PUBLIC_KEY.get() {
        return key.clone();
    }
    YOUR_PUBLIC_KEY.to_string()
}

#[component]
fn CodePage(
    name: String,
    public_key: String,
    on_name: EventHandler<String>,
    on_back: EventHandler<()>,
) -> Element {
    let payload = contact_code(&name, &public_key);
    let mark = code_mark(&payload);
    let copied = use_signal(|| false);
    let share = payload.clone();
    let key = public_key.clone();
    rsx! {
        div { class: "page",
            div { class: "title-row",
                button { class: "back", onclick: move |_| on_back.call(()), "←" }
                h1 { "Your QR code" }
            }
            input {
                class: "name-line hand",
                aria_label: "Your name",
                value: "{name}",
                oninput: move |event| on_name.call(event.value()),
            }
            Mark {
                mark,
                on_copy: move |_| copy_text(share.clone(), copied),
            }
            div { class: "field",
                span { "Public key" }
                button {
                    class: "public-key",
                    onclick: move |_| copy_text(key.clone(), copied),
                    "{public_key}"
                }
            }
            if copied() {
                p { class: "notice", "Copied" }
            }
        }
    }
}

#[component]
fn Mark(mark: Option<(usize, Vec<(usize, usize)>)>, on_copy: EventHandler<()>) -> Element {
    let Some((width, cells)) = mark else {
        return rsx! {};
    };
    rsx! {
        button {
            class: "mark",
            aria_label: "Copy your QR code",
            onclick: move |_| on_copy.call(()),
            svg {
                view_box: "0 0 {width} {width}",
                xmlns: "http://www.w3.org/2000/svg",
                for (x, y) in cells {
                    rect { x: "{x}", y: "{y}", width: "1", height: "1" }
                }
            }
        }
    }
}

/// The clipboard text for a note: the highlighted portion, or the whole note when nothing is highlighted.
fn copied_text(full: &str, selected: &str) -> String {
    let selected = selected.trim();
    if selected.is_empty() {
        full.trim().to_string()
    } else {
        selected.to_string()
    }
}

/// The highlighted text when it sits inside this note. Anything else is empty.
async fn selection_inside(note_id: u64) -> String {
    let script = format!(
        r#"(function () {{
  var node = document.getElementById("note-{note_id}");
  var sel = window.getSelection && window.getSelection();
  if (!node || !sel || sel.rangeCount === 0 || sel.isCollapsed) {{
    dioxus.send("");
    return;
  }}
  var inside = false;
  for (var i = 0; i < sel.rangeCount; i++) {{
    var range = sel.getRangeAt(i);
    if (node.contains(range.startContainer) && node.contains(range.endContainer)) inside = true;
  }}
  dioxus.send(inside ? sel.toString() : "");
}})();"#
    );
    let mut eval = document::eval(&script);
    eval.recv::<String>().await.unwrap_or_default()
}

async fn put_on_clipboard(text: &str) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        write_clipboard(text).await
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        write_clipboard(text)
    }
}

fn copy_text(text: String, mut copied: Signal<bool>) {
    #[cfg(target_arch = "wasm32")]
    {
        spawn(async move {
            if write_clipboard(&text).await {
                copied.set(true);
            }
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if write_clipboard(&text) {
            copied.set(true);
        }
    }
}

#[cfg(target_arch = "wasm32")]
async fn write_clipboard(text: &str) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    wasm_bindgen_futures::JsFuture::from(window.navigator().clipboard().write_text(text))
        .await
        .is_ok()
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn write_clipboard(text: &str) -> bool {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text))
        .is_ok()
}

#[cfg(target_os = "android")]
fn write_clipboard(text: &str) -> bool {
    session::copy_to_clipboard(text)
}

#[cfg(not(target_os = "android"))]
#[component]
fn PhotoFile(on_scanned: EventHandler<(String, String)>, notice: Signal<String>) -> Element {
    rsx! {
        label { class: "photo",
            "Take a photo"
            input {
                r#type: "file",
                accept: "image/*",
                capture: "environment",
                aria_label: "Take a photo of their QR code",
                onchange: move |event| {
                    let Some(file) = event.files().into_iter().next() else {
                        return;
                    };
                    let on_scanned = on_scanned;
                    let mut notice = notice;
                    spawn(async move {
                        let Ok(bytes) = file.read_bytes().await else {
                            notice.set(UNREADABLE.into());
                            return;
                        };
                        accept_image(&bytes, on_scanned, notice);
                    });
                },
            }
        }
    }
}

#[component]
fn PhotoControl(on_scanned: EventHandler<(String, String)>, mut notice: Signal<String>) -> Element {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "windows"))]
    {
        let mut scanning = use_signal(|| false);
        let mut frame = use_signal(String::new);
        #[cfg(target_os = "windows")]
        let mut use_file = use_signal(|| false);
        #[cfg(target_os = "windows")]
        let show_file = use_file();
        #[cfg(not(target_os = "windows"))]
        let show_file = false;
        use_drop(|| viewfinder::stop());
        let picture = frame();
        return rsx! {
            if scanning() {
                div { class: if cfg!(target_os = "ios") { "finder rear" } else { "finder" },
                    div { class: "well",
                        if picture.is_empty() {
                            p { class: "opening", "Opening the camera…" }
                        } else {
                            img { src: "{picture}", alt: "Camera" }
                        }
                    }
                    p { class: "aim", "Point at their code" }
                    button {
                        class: "photo",
                        onclick: move |_| {
                            viewfinder::stop();
                            scanning.set(false);
                            frame.set(String::new());
                        },
                        "Close the camera"
                    }
                }
            } else {
                button {
                    class: "photo",
                    aria_label: "Scan their QR code",
                    onclick: move |_| {
                        notice.set(String::new());
                        frame.set(String::new());
                        scanning.set(true);
                        let token = viewfinder::start();
                        let on_scanned = on_scanned;
                        spawn(async move {
                            loop {
                                if viewfinder::token() != token {
                                    break;
                                }
                                match viewfinder::look() {
                                    viewfinder::View::Picture(url) => {
                                        if frame() != url {
                                            frame.set(url);
                                        }
                                    }
                                    viewfinder::View::Code(text) => {
                                        viewfinder::stop();
                                        scanning.set(false);
                                        frame.set(String::new());
                                        if let Some(contact) = contact_from_code(&text) {
                                            on_scanned.call(contact);
                                        } else {
                                            notice.set(UNREADABLE.into());
                                        }
                                        break;
                                    }
                                    viewfinder::View::Denied => {
                                        viewfinder::stop();
                                        scanning.set(false);
                                        frame.set(String::new());
                                        #[cfg(target_os = "windows")]
                                        use_file.set(true);
                                        notice.set("N10A needs the camera to photograph a code.".into());
                                        break;
                                    }
                                    viewfinder::View::Unavailable => {
                                        viewfinder::stop();
                                        scanning.set(false);
                                        frame.set(String::new());
                                        #[cfg(target_os = "windows")]
                                        use_file.set(true);
                                        notice.set("The camera didn't open.".into());
                                        break;
                                    }
                                    viewfinder::View::Starting => {}
                                }
                                session::rest(80).await;
                            }
                        });
                    },
                    "Scan their code"
                }
                if show_file {
                    PhotoFile { on_scanned, notice }
                }
            }
        };
    }
    #[cfg(target_os = "android")]
    {
        return rsx! {
            button {
                class: "photo",
                aria_label: "Scan their QR code",
                onclick: move |_| {
                    notice.set(String::new());
                    let on_scanned = on_scanned;
                    spawn(async move {
                        match session::capture_photo().await {
                            session::PhotoCapture::Code(text) => {
                                if let Some(contact) = contact_from_code(&text) {
                                    on_scanned.call(contact);
                                } else {
                                    notice.set(UNREADABLE.into());
                                }
                            }
                            session::PhotoCapture::Denied => {
                                notice.set("N10A needs the camera to photograph a code.".into());
                            }
                            session::PhotoCapture::Unavailable => {
                                notice.set("The camera didn't open.".into());
                            }
                            session::PhotoCapture::Cancelled => {}
                        }
                    });
                },
                "Scan their code"
            }
        };
    }
    #[cfg(not(any(
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    )))]
    rsx! {
        PhotoFile { on_scanned, notice }
    }
}

#[component]
fn AddPage(
    name: String,
    code: String,
    on_name: EventHandler<String>,
    on_code: EventHandler<String>,
    on_back: EventHandler<()>,
    on_scanned: EventHandler<(String, String)>,
    on_save: EventHandler<()>,
) -> Element {
    let notice = use_signal(String::new);
    let message = notice();
    rsx! {
        div {
            class: "page",
            tabindex: "0",
            onmounted: move |event| {
                spawn(async move {
                    let _ = event.set_focus(true).await;
                });
            },
            onpaste: move |event| accept_paste(event, on_scanned, notice),
            div { class: "title-row",
                button { class: "back", aria_label: "Back to contacts", onclick: move |_| on_back.call(()), "←" }
                h1 { "Add a contact" }
            }
            PhotoControl { on_scanned, notice }
            p { class: "hint", "Or paste a photo of their QR code." }
            if !message.is_empty() {
                p { class: "notice", "{message}" }
            }
            label { class: "field",
                span { "Name" }
                input {
                    class: "hand",
                    value: "{name}",
                    oninput: move |event| on_name.call(event.value()),
                }
            }
            label { class: "field",
                span { "Public key" }
                input {
                    value: "{code}",
                    oninput: move |event| on_code.call(event.value()),
                }
            }
            button { class: "save", onclick: move |_| on_save.call(()), "Add to your contacts" }
        }
    }
}

const UNREADABLE: &str = "That photo doesn't show a code.";

#[cfg_attr(any(target_os = "macos", target_os = "ios"), allow(dead_code))]
fn accept_image(
    bytes: &[u8],
    on_scanned: EventHandler<(String, String)>,
    mut notice: Signal<String>,
) {
    match contact_from_image(bytes) {
        Some((name, identity)) => on_scanned.call((name, identity)),
        None => notice.set(UNREADABLE.into()),
    }
}

fn accept_paste(
    event: Event<ClipboardData>,
    on_scanned: EventHandler<(String, String)>,
    mut notice: Signal<String>,
) {
    event.prevent_default();
    if let Some(text) = pasted_text(&event) {
        if let Some((name, identity)) = contact_from_code(&text) {
            on_scanned.call((name, identity));
            return;
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let Some(blob) = pasted_image_blob(&event) else {
            notice.set(UNREADABLE.into());
            return;
        };
        spawn(async move {
            let Some(bytes) = read_blob(blob).await else {
                notice.set(UNREADABLE.into());
                return;
            };
            accept_image(&bytes, on_scanned, notice);
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = event;
        match native_clipboard_contact() {
            Some((name, identity)) => on_scanned.call((name, identity)),
            None => notice.set(UNREADABLE.into()),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn pasted_text(_event: &Event<ClipboardData>) -> Option<String> {
    None
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn native_clipboard_contact() -> Option<(String, String)> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    if let Ok(text) = clipboard.get_text() {
        if let Some(contact) = contact_from_code(&text) {
            return Some(contact);
        }
    }
    let image = clipboard.get_image().ok()?;
    contact_from_rgba(image.width as u32, image.height as u32, &image.bytes)
}

#[cfg(target_os = "android")]
fn native_clipboard_contact() -> Option<(String, String)> {
    let contents = session::read_clipboard()?;
    if let Some(text) = contents.text.as_deref() {
        if let Some(contact) = contact_from_code(text) {
            return Some(contact);
        }
    }
    contact_from_image(contents.image.as_deref()?)
}

#[cfg(target_arch = "wasm32")]
fn pasted_text(event: &Event<ClipboardData>) -> Option<String> {
    let data = clipboard_data(event)?;
    let text = data.get_data("text/plain").ok()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(target_arch = "wasm32")]
fn pasted_image_blob(event: &Event<ClipboardData>) -> Option<web_sys::Blob> {
    let data = clipboard_data(event)?;
    let items = data.items();
    for index in 0..items.length() {
        let item = items.get(index)?;
        if item.kind() == "file" && item.type_().starts_with("image/") {
            let file = item.get_as_file().ok()??;
            return Some(file.into());
        }
    }
    None
}

#[cfg(target_arch = "wasm32")]
fn clipboard_data(event: &Event<ClipboardData>) -> Option<web_sys::DataTransfer> {
    use wasm_bindgen::JsCast;
    let raw = event.data().downcast::<web_sys::Event>()?.clone();
    let clipboard = raw.dyn_into::<web_sys::ClipboardEvent>().ok()?;
    clipboard.clipboard_data()
}

#[cfg(target_arch = "wasm32")]
async fn read_blob(blob: web_sys::Blob) -> Option<Vec<u8>> {
    let buffer = wasm_bindgen_futures::JsFuture::from(blob.array_buffer())
        .await
        .ok()?;
    Some(js_sys::Uint8Array::new(&buffer).to_vec())
}

fn once_per_tap(last: &std::cell::Cell<u128>) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    if now.saturating_sub(last.get()) < 80 {
        return false;
    }
    last.set(now);
    true
}

fn toggle_embeds(mut embeds: Signal<bool>, last: &std::cell::Cell<u128>) {
    if once_per_tap(last) {
        embeds.set(!embeds());
    }
}

fn begin_place(
    last: &std::cell::Cell<u128>,
    mut embeds: Signal<bool>,
    mut place_epoch: Signal<u64>,
    mut embed_notice: Signal<String>,
    on_place: EventHandler<Place>,
) {
    if !once_per_tap(last) {
        return;
    }
    embeds.set(false);
    let epoch = place_epoch() + 1;
    place_epoch.set(epoch);
    embed_notice.set("Finding this location…".into());
    spawn(async move {
        let found = location::current_place().await;
        if place_epoch() != epoch {
            return;
        }
        match found {
            Some(place) => {
                embed_notice.set(String::new());
                on_place.call(place);
            }
            None => {
                embed_notice.set("Location isn't available.".into());
            }
        }
    });
}

#[component]
fn JournalPage(
    person: Person,
    draft: String,
    voice: VoiceLine,
    film_at: u32,
    reach: Reach,
    place_epoch: Signal<u64>,
    tail: u64,
    transfer: TransferFace,
    landed: u64,
    on_draft: EventHandler<String>,
    on_place: EventHandler<Place>,
    on_back: EventHandler<()>,
    on_react: EventHandler<(u64, Reaction)>,
    on_retry: EventHandler<u64>,
    on_write: EventHandler<Option<(String, Vec<u8>)>>,
) -> Element {
    let name = person.name.clone();
    let groups = group_by_day(&person.notes);
    let hearing = hearing_file(&person.identity, &transfer);
    let incoming_on = if hearing {
        person.notes.iter().rev().find_map(|note| {
            (!note.mine && note.file_name.as_ref().is_some_and(|name| !name.is_empty()))
                .then_some(note.id)
        })
    } else {
        None
    };
    let real = person.identity.trim().len() == 128;
    let ways = if real {
        if reach.pending {
            OfferedWays::none()
        } else {
            reach.ways
        }
    } else {
        offered_ways(person.id)
    };
    let can_note = if real { true } else { ways.note };
    let can_voice = ways.voice && can_call(&person);
    let identity = person.identity.clone();
    let on_this = this_call(&identity, &voice);
    let kind = voice.kind;
    let on_call = on_this && (kind == 1 || kind == 2);
    let ending = on_call && !voice.video;
    let label = if ending { "end" } else { "voice" };
    let video_up = on_call && voice.video;
    let video_label = if video_up { "end" } else { "video" };
    let video_identity = identity.clone();
    let _refresh = film_at;
    let film = shown_picture(video_up);
    let hint = if on_this && kind == 4 {
        "is calling".to_string()
    } else if on_this && kind != 0 {
        voice.words.clone()
    } else {
        String::new()
    };
    let embeds = use_signal(|| false);
    let picked_name = use_signal(String::new);
    let picked_bytes = use_signal(Vec::<u8>::new);
    let plus_at = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(0u128)));
    let place_at = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(0u128)));
    let embed_notice = use_signal(String::new);
    let mut mounted = use_signal(|| None::<std::rc::Rc<MountedData>>);
    let pinned = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(true)));
    let seen = use_hook(|| std::rc::Rc::new(std::cell::Cell::new((person.notes.len(), tail))));
    let count = person.notes.len();
    let previous = seen.get();
    if (count, tail) != previous {
        let from_send = tail != previous.1;
        let follow = from_send || pinned.get();
        seen.set((count, tail));
        if follow {
            pinned.set(true);
            if let Some(data) = mounted.read().as_ref().cloned() {
                jump_to_latest(data);
            }
        }
    }
    let under_name = path_label(real, reach.pending, reach.winner.as_deref(), reach.hops);
    let show_voice = (ending || can_voice) && !video_up && !(on_this && kind == 4);
    let show_video = (video_up || ways.video) && !ending && !(on_this && kind == 4);
    let write_from_key = on_write.clone();
    let write_from_button = on_write;
    rsx! {
        div { class: "page journal",
            div { class: "title-row",
                button { class: "back", aria_label: "Back to contacts", onclick: move |_| on_back.call(()), "←" }
                div { class: "who",
                    h1 { "{name}" }
                    if !under_name.is_empty() {
                        p { class: "via", "{under_name}" }
                    }
                }
            }
            div {
                class: "log",
                onmounted: {
                    let pinned = pinned.clone();
                    move |event| {
                        let data = event.data.clone();
                        mounted.set(Some(data.clone()));
                        pinned.set(true);
                        jump_to_latest(data);
                    }
                },
                onscroll: {
                    let pinned = pinned.clone();
                    move |event| {
                        let data = event.data();
                        let gap = f64::from(data.scroll_height())
                            - data.scroll_top()
                            - f64::from(data.client_height());
                    pinned.set(gap < 80.0);
                }
            },
            ul { class: "thread",
                for (day, notes) in groups {
                    li { class: "day", "{day}" }
                    for note in notes {
                        NoteRow {
                            note_id: note.id,
                            kind: note.kind,
                            words: note.words,
                            time: note.time,
                            mine: note.mine,
                            settled: note.settled,
                            failed: note.failed,
                            posted: note.posted,
                            downloaded: note.downloaded,
                            place: note.place,
                            file_name: note.file_name.clone(),
                            reaction: note.reaction,
                            can_react: !note.mine && note.kind == NoteKind::Text,
                            on_react: move |choice| on_react.call((note.id, choice)),
                            on_retry: move |_| on_retry.call(note.id),
                            landed: landed == note.id,
                            meter: if note.mine
                                && note.id == transfer.note_id
                                && !note.settled
                                && !note.failed
                                && !note.posted
                            {
                                Some((transfer.done, transfer.total))
                            } else if incoming_on == Some(note.id) {
                                Some((transfer.incoming_done, transfer.incoming_total))
                            } else {
                                None
                            },
                        }
                    }
                }
                if hearing && incoming_on.is_none() {
                    li { class: "note theirs waiting",
                        div { class: "note-body",
                            div { class: "pack",
                                Meter {
                                    done: transfer.incoming_done,
                                    total: transfer.incoming_total,
                                    label: "Receiving",
                                }
                            }
                        }
                    }
                }
            }
            div { class: "dock",
                if film.is_some() || !hint.is_empty() {
                    div { class: "call-line",
                        if let Some(film) = film {
                            img { class: "film", src: "{film}", alt: "video" }
                        }
                        if !hint.is_empty() {
                            p { class: "hint", "{hint}" }
                        }
                    }
                }
                div { class: "write-row",
                    if can_note {
                        div { class: "note-line",
                        div { class: "embed",
                            div {
                                class: "plus",
                                role: "button",
                                aria_label: "Add something to this note",
                                aria_expanded: embeds(),
                                onpointerdown: move |event| event.prevent_default(),
                                ontouchstart: move |event| event.prevent_default(),
                                onpointerup: {
                                    let plus_at = plus_at.clone();
                                    move |event| {
                                        event.prevent_default();
                                        toggle_embeds(embeds, &plus_at);
                                    }
                                },
                                ontouchend: {
                                    let plus_at = plus_at.clone();
                                    move |event| {
                                        event.prevent_default();
                                        toggle_embeds(embeds, &plus_at);
                                    }
                                },
                                "+"
                            }
                            if embeds() {
                                div { class: "embed-list",
                                    div {
                                        class: "choice",
                                        role: "button",
                                        onpointerdown: move |event| event.prevent_default(),
                                        ontouchstart: move |event| event.prevent_default(),
                                        onpointerup: {
                                            let place_at = place_at.clone();
                                            move |event| {
                                                event.prevent_default();
                                                begin_place(place_at.as_ref(), embeds, place_epoch, embed_notice, on_place);
                                            }
                                        },
                                        ontouchend: {
                                            let place_at = place_at.clone();
                                            move |event| {
                                                event.prevent_default();
                                                begin_place(place_at.as_ref(), embeds, place_epoch, embed_notice, on_place);
                                            }
                                        },
                                        "location"
                                    }
                                    {file_choice_row(file_choice(reach.file_limit), reach.file_limit, picked_name, picked_bytes, embeds, embed_notice)}
                                }
                            }
                        }
                        div { class: "composer", "data-value": "{draft}",
                            textarea {
                                class: "hand",
                                aria_label: "Write a note",
                                placeholder: "Write a note",
                                rows: "1",
                                value: "{draft}",
                                onmounted: move |event| {
                                    spawn(async move {
                                        let _ = event.set_focus(true).await;
                                    });
                                },
                                oninput: move |event| on_draft.call(event.value()),
                                onkeydown: move |event| {
                                    if event.key() == Key::Enter && !event.modifiers().shift() {
                                        event.prevent_default();
                                        post_draft(picked_name, picked_bytes, embeds, embed_notice, &write_from_key);
                                    }
                                },
                            }
                        }
                        }
                    }
                    if show_voice || show_video || can_note {
                        div { class: "actions",
                            if show_voice || show_video {
                                div { class: "offers",
                                    if show_voice {
                                        button {
                                            onclick: move |_| {
                                                #[cfg(not(target_arch = "wasm32"))]
                                                {
                                                    if ending {
                                                        session::hang_up();
                                                    } else {
                                                        session::place_call(&identity);
                                                    }
                                                }
                                                #[cfg(target_arch = "wasm32")]
                                                {
                                                    let _ = (&identity, ending);
                                                }
                                            },
                                            "{label}"
                                        }
                                    }
                                    if show_video {
                                        button {
                                            onclick: move |_| {
                                                #[cfg(any(target_os = "macos", target_os = "android"))]
                                                {
                                                    if video_up {
                                                        session::hang_up();
                                                    } else if !ending {
                                                        session::place_video(&video_identity);
                                                    }
                                                }
                                                #[cfg(not(any(target_os = "macos", target_os = "android")))]
                                                {
                                                    let _ = (&video_identity, video_up, ending);
                                                }
                                            },
                                            "{video_label}"
                                        }
                                    }
                                }
                            }
                            if can_note {
                                button {
                                    class: "send",
                                    aria_label: "Send this note",
                                    onclick: move |_| {
                                        post_draft(picked_name, picked_bytes, embeds, embed_notice, &write_from_button);
                                    },
                                    "send"
                                }
                            }
                        }
                    }
                }
                if can_note && !picked_name().is_empty() {
                    p { class: "attached", "{picked_name}" }
                }
                if can_note && !embed_notice().is_empty() {
                    p { class: "hint", "{embed_notice}" }
                }
            }
            }
        }
    }
}

fn post_draft(
    mut picked_name: Signal<String>,
    mut picked_bytes: Signal<Vec<u8>>,
    mut embeds: Signal<bool>,
    mut embed_notice: Signal<String>,
    on_write: &EventHandler<Option<(String, Vec<u8>)>>,
) {
    let name = picked_name();
    let bytes = picked_bytes();
    picked_name.set(String::new());
    picked_bytes.set(Vec::new());
    let file = if name.is_empty() || bytes.is_empty() {
        None
    } else {
        Some((name, bytes))
    };
    on_write.call(file);
    embeds.set(false);
    embed_notice.set(String::new());
}

enum HeldFile {
    Ready(String, Vec<u8>),
    Over,
    Unreadable,
}

fn judge_file(name: &str, bytes: &[u8], limit: Option<u64>) -> HeldFile {
    if bytes.is_empty() || u32::try_from(bytes.len()).is_err() {
        return HeldFile::Unreadable;
    }
    if limit.is_some_and(|limit| bytes.len() as u64 > limit) {
        return HeldFile::Over;
    }
    HeldFile::Ready(file_name_of(name), bytes.to_vec())
}

fn store_pick(
    pick: HeldFile,
    limit: Option<u64>,
    mut picked_name: Signal<String>,
    mut picked_bytes: Signal<Vec<u8>>,
    mut embeds: Signal<bool>,
    mut notice: Signal<String>,
) {
    match pick {
        HeldFile::Ready(name, bytes) => {
            picked_name.set(name);
            picked_bytes.set(bytes);
            notice.set(String::new());
            embeds.set(false);
        }
        HeldFile::Over => {
            notice.set(file_too_big(limit));
            embeds.set(false);
        }
        HeldFile::Unreadable => {
            notice.set("That file could not be read.".into());
            embeds.set(false);
        }
    }
}

#[cfg(target_os = "android")]
fn file_choice_row(
    label: String,
    limit: Option<u64>,
    picked_name: Signal<String>,
    picked_bytes: Signal<Vec<u8>>,
    embeds: Signal<bool>,
    notice: Signal<String>,
) -> Element {
    rsx! {
        div {
            class: "choice",
            role: "button",
            onclick: move |_| {
                let picked_name = picked_name;
                let picked_bytes = picked_bytes;
                let embeds = embeds;
                let notice = notice;
                spawn(async move {
                    match session::choose_file(limit).await {
                        session::ChosenFile::Ready { name, bytes } => {
                            store_pick(judge_file(&name, &bytes, limit), limit, picked_name, picked_bytes, embeds, notice);
                        }
                        session::ChosenFile::Over => {
                            store_pick(HeldFile::Over, limit, picked_name, picked_bytes, embeds, notice);
                        }
                        session::ChosenFile::Missing => {
                            store_pick(HeldFile::Unreadable, limit, picked_name, picked_bytes, embeds, notice);
                        }
                        session::ChosenFile::Cancelled => {}
                    }
                });
            },
            "{label}"
        }
    }
}

#[cfg(target_os = "macos")]
fn file_choice_row(
    label: String,
    limit: Option<u64>,
    picked_name: Signal<String>,
    picked_bytes: Signal<Vec<u8>>,
    embeds: Signal<bool>,
    notice: Signal<String>,
) -> Element {
    rsx! {
        div {
            class: "choice",
            role: "button",
            onclick: move |_| {
                let Some(path) = mac_file() else {
                    return;
                };
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".into());
                let announced = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
                if limit.is_some_and(|limit| announced > limit && announced != 0) {
                    store_pick(HeldFile::Over, limit, picked_name, picked_bytes, embeds, notice);
                    return;
                }
                match std::fs::read(&path) {
                    Ok(bytes) => store_pick(
                        judge_file(&name, &bytes, limit),
                        limit,
                        picked_name,
                        picked_bytes,
                        embeds,
                        notice,
                    ),
                    Err(_) => store_pick(
                        HeldFile::Unreadable,
                        limit,
                        picked_name,
                        picked_bytes,
                        embeds,
                        notice,
                    ),
                }
            },
            "{label}"
        }
    }
}

#[cfg(target_os = "macos")]
fn mac_file() -> Option<std::path::PathBuf> {
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg("POSIX path of (choose file with prompt \"Choose a file\")")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    if path.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(path))
    }
}

#[cfg(not(any(target_os = "android", target_os = "macos")))]
fn file_choice_row(
    label: String,
    limit: Option<u64>,
    picked_name: Signal<String>,
    picked_bytes: Signal<Vec<u8>>,
    embeds: Signal<bool>,
    notice: Signal<String>,
) -> Element {
    rsx! {
        label {
            class: "choice",
            "{label}"
            input {
                r#type: "file",
                onchange: move |event| {
                    let files = event.files();
                    let Some(file) = files.into_iter().next() else {
                        return;
                    };
                    let announced = file.size();
                    if limit.is_some_and(|limit| announced > limit && announced != 0) {
                        store_pick(HeldFile::Over, limit, picked_name, picked_bytes, embeds, notice);
                        return;
                    }
                    let name = file.name();
                    let picked_name = picked_name;
                    let picked_bytes = picked_bytes;
                    let embeds = embeds;
                    let notice = notice;
                    spawn(async move {
                        match file.read_bytes().await {
                            Ok(bytes) => store_pick(
                                judge_file(&name, &bytes, limit),
                                limit,
                                picked_name,
                                picked_bytes,
                                embeds,
                                notice,
                            ),
                            Err(_) => store_pick(
                                HeldFile::Unreadable,
                                limit,
                                picked_name,
                                picked_bytes,
                                embeds,
                                notice,
                            ),
                        }
                    });
                },
            }
        }
    }
}

const REVEAL: f64 = 72.0;

/// "In downloads" stays beside that file's name for three seconds after it lands.
#[cfg(not(target_arch = "wasm32"))]
fn show_download_toast(mut toast: Signal<(u64, u64)>, note_id: u64) {
    let stamp = toast().1.wrapping_add(1).max(1);
    toast.set((note_id, stamp));
    spawn(async move {
        session::rest(3_000).await;
        if toast().1 == stamp {
            toast.set((0, stamp));
        }
    });
}

fn jump_to_latest(data: std::rc::Rc<MountedData>) {
    spawn(async move {
        let reveal = || {
            let data = data.clone();
            async move {
                let _ = data
                    .scroll(
                        dioxus::html::geometry::PixelsVector2D::new(0.0, 1.0e9),
                        ScrollBehavior::Instant,
                    )
                    .await;
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            session::rest(50).await;
            reveal().await;
            session::rest(200).await;
        }
        reveal().await;
    });
}

#[component]
fn NoteRow(
    note_id: u64,
    kind: NoteKind,
    words: String,
    time: String,
    mine: bool,
    settled: bool,
    failed: bool,
    posted: bool,
    downloaded: bool,
    place: Option<Place>,
    file_name: Option<String>,
    reaction: Option<Reaction>,
    can_react: bool,
    on_react: EventHandler<Reaction>,
    on_retry: EventHandler<()>,
    landed: bool,
    meter: Option<(u64, u64)>,
) -> Element {
    let mut reveal = use_signal(|| 0.0);
    let mut drag = use_signal(|| None::<(f64, f64, f64)>);
    let mut choosing = use_signal(|| false);
    let mut copied = use_signal(|| false);
    let mut text_down = use_signal(|| None::<(f64, f64)>);
    let words_class = if kind == NoteKind::Text {
        "words hand"
    } else {
        "words"
    };
    let side = if mine { "mine" } else { "theirs" };
    let tone = if settled { "" } else { "waiting" };
    let shown = shown_words(&words, place);
    let can_copy = kind == NoteKind::Text && !shown.is_empty();
    let meter_label = if mine { "Sending" } else { "Receiving" };
    let show_failed = failed && !downloaded && meter.is_none();
    rsx! {
        li { class: "note {note_class(kind)} {side} {tone}",
            div {
                class: if drag().is_some() { "swipe live" } else { "swipe" },
                style: "--reveal: {reveal()}px",
                onpointerdown: move |event| {
                    let point = event.client_coordinates();
                    drag.set(Some((point.x, point.y, reveal())));
                },
                onpointermove: move |event| {
                    let Some((start_x, _, start_reveal)) = drag() else {
                        return;
                    };
                    let next = (start_reveal + start_x - event.client_coordinates().x)
                        .clamp(0.0, REVEAL);
                    reveal.set(next);
                },
                onpointerup: move |event| {
                    let tap = drag().is_some_and(|(start_x, start_y, _)| {
                        let point = event.client_coordinates();
                        let dx = point.x - start_x;
                        let dy = point.y - start_y;
                        dx * dx + dy * dy < 18.0 * 18.0
                    });
                    release_note(drag, reveal);
                    if tap && (can_react || can_copy) {
                        copied.set(false);
                        choosing.set(!choosing());
                    }
                },
                onpointercancel: move |_| release_note(drag, reveal),
                onpointerleave: move |_| release_note(drag, reveal),
                div { class: "note-body",
                    if let Some(place) = place {
                        PlaceMap { place }
                    }
                    div { class: "pack",
                        if !shown.is_empty() {
                            p {
                                id: "note-{note_id}",
                                class: "{words_class}",
                                onpointerdown: move |event| {
                                    event.stop_propagation();
                                    let point = event.client_coordinates();
                                    text_down.set(Some((point.x, point.y)));
                                },
                                onpointerup: move |event| {
                                    event.stop_propagation();
                                    let point = event.client_coordinates();
                                    let small = text_down().is_some_and(|(x, y)| {
                                        let dx = point.x - x;
                                        let dy = point.y - y;
                                        dx * dx + dy * dy < 18.0 * 18.0
                                    });
                                    text_down.set(None);
                                    spawn(async move {
                                        let selected = selection_inside(note_id).await;
                                        if !selected.trim().is_empty() {
                                            choosing.set(true);
                                        } else if small && (can_react || can_copy) {
                                            copied.set(false);
                                            choosing.set(!choosing());
                                        }
                                    });
                                },
                                onpointercancel: move |_| text_down.set(None),
                                "{shown}"
                            }
                        }
                        if let Some(name) = file_name.clone().filter(|name| !name.is_empty()) {
                            div { class: "file-line",
                                if failed && !mine {
                                    span { class: "attached", "{name}" }
                                } else {
                                    button {
                                        class: "attached",
                                        aria_label: "Open {name}",
                                        onpointerdown: move |event| event.stop_propagation(),
                                        onpointerup: move |event| event.stop_propagation(),
                                        onclick: {
                                            let name = name.clone();
                                            move |event| {
                                                event.stop_propagation();
                                                #[cfg(not(target_arch = "wasm32"))]
                                                session::show_kept_file(note_id, &name);
                                                #[cfg(target_arch = "wasm32")]
                                                let _ = (note_id, &name);
                                            }
                                        },
                                        "{name}"
                                    }
                                }
                                if landed {
                                    span { class: "toast", "In downloads" }
                                }
                            }
                        }
                        if let Some((done, total)) = meter {
                            Meter {
                                done,
                                total,
                                label: meter_label,
                            }
                        }
                        if posted && !settled {
                            div { class: "posted",
                                span { "Posted" }
                            }
                        } else if show_failed {
                            div { class: "failed",
                                if mine {
                                    button {
                                        aria_label: "Failed. Tap to try again.",
                                        onpointerdown: move |event| event.stop_propagation(),
                                        onpointerup: move |event| event.stop_propagation(),
                                        onclick: move |event| {
                                            event.stop_propagation();
                                            on_retry.call(());
                                        },
                                        "Failed"
                                    }
                                } else {
                                    span { aria_label: "Failed", "Failed" }
                                }
                            }
                        } else if let Some(reaction) = reaction.filter(|_| !(choosing() && can_react)) {
                            span { class: "reaction", aria_label: "Reaction",
                                ReactionGlyph { kind: reaction }
                            }
                        }
                        if choosing() && (can_react || can_copy) {
                            ReactChoices {
                                chosen: reaction,
                                marks: can_react,
                                copied: copied(),
                                on_react: move |choice| {
                                    choosing.set(false);
                                    on_react.call(choice);
                                },
                                on_copy: move |_| {
                                    let full = shown.clone();
                                    spawn(async move {
                                        let selected = selection_inside(note_id).await;
                                        let text = copied_text(&full, &selected);
                                        if text.is_empty() {
                                            return;
                                        }
                                        if put_on_clipboard(&text).await {
                                            copied.set(true);
                                        }
                                    });
                                },
                            }
                        }
                    }
                }
                span { class: "when",
                    span { "{time}" }
                }
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TransferFace {
    note_id: u64,
    done: u64,
    total: u64,
    incoming_identity: String,
    incoming_done: u64,
    incoming_total: u64,
}

impl TransferFace {
    #[cfg(not(target_arch = "wasm32"))]
    fn from_session(view: session::TransferView) -> Self {
        Self {
            note_id: view.note_id,
            done: view.done,
            total: view.total,
            incoming_identity: view.incoming_identity,
            incoming_done: view.incoming_done,
            incoming_total: view.incoming_total,
        }
    }
}

fn hearing_file(identity: &str, transfer: &TransferFace) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        transfer.incoming_total > 0 && session::same_contact(identity, &transfer.incoming_identity)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (identity, transfer);
        false
    }
}

#[component]
fn Meter(done: u64, total: u64, label: &'static str) -> Element {
    let width = if total == 0 {
        0
    } else {
        (done.saturating_mul(100) / total).min(100)
    };
    rsx! {
        div {
            class: "meter",
            role: "progressbar",
            aria_label: "{label}",
            aria_valuemin: "0",
            aria_valuemax: "100",
            aria_valuenow: "{width}",
            span { style: "width: {width}%;" }
        }
    }
}

#[component]
fn ReactChoices(
    chosen: Option<Reaction>,
    marks: bool,
    copied: bool,
    on_react: EventHandler<Reaction>,
    on_copy: EventHandler<()>,
) -> Element {
    rsx! {
        div {
            class: "reacts",
            onpointerdown: move |event| event.stop_propagation(),
            onpointerup: move |event| event.stop_propagation(),
            if marks {
                button {
                class: if chosen == Some(Reaction::Love) { "chosen" } else { "" },
                aria_label: "Love",
                onclick: move |event| {
                    event.stop_propagation();
                    on_react.call(Reaction::Love);
                },
                ReactionGlyph { kind: Reaction::Love }
            }
            button {
                class: if chosen == Some(Reaction::Yes) { "chosen" } else { "" },
                aria_label: "Yes",
                onclick: move |event| {
                    event.stop_propagation();
                    on_react.call(Reaction::Yes);
                },
                ReactionGlyph { kind: Reaction::Yes }
            }
            button {
                class: if chosen == Some(Reaction::Down) { "chosen" } else { "" },
                aria_label: "No",
                onclick: move |event| {
                    event.stop_propagation();
                    on_react.call(Reaction::Down);
                },
                ReactionGlyph { kind: Reaction::Down }
            }
            button {
                class: if chosen == Some(Reaction::Ha) { "chosen" } else { "" },
                aria_label: "Laugh",
                onclick: move |event| {
                    event.stop_propagation();
                    on_react.call(Reaction::Ha);
                },
                ReactionGlyph { kind: Reaction::Ha }
            }
            button {
                class: if chosen == Some(Reaction::Oh) { "chosen" } else { "" },
                aria_label: "Oh",
                onclick: move |event| {
                    event.stop_propagation();
                    on_react.call(Reaction::Oh);
                },
                ReactionGlyph { kind: Reaction::Oh }
            }
            button {
                class: if chosen == Some(Reaction::Ask) { "chosen" } else { "" },
                aria_label: "Question",
                onclick: move |event| {
                    event.stop_propagation();
                    on_react.call(Reaction::Ask);
                },
                ReactionGlyph { kind: Reaction::Ask }
                }
            }
            button {
                class: "copy",
                aria_label: "Copy",
                onclick: move |event| {
                    event.stop_propagation();
                    on_copy.call(());
                },
                if copied { "Copied" } else { "Copy" }
            }
        }
    }
}

#[component]
/// Black line drawings from [OpenMoji](https://openmoji.org), CC BY-SA 4.0.
fn ReactionGlyph(kind: Reaction) -> Element {
    rsx! {
        svg {
            view_box: "0 0 72 72",
            xmlns: "http://www.w3.org/2000/svg",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            match kind {
                Reaction::Love => rsx! {
                    path { d: "m59.5 25c0-6.904-5.596-12.5-12.5-12.5-4.753 0-8.886 2.654-11 6.56-2.114-3.906-6.247-6.56-11-6.56-6.904 0-12.5 5.596-12.5 12.5 0 2.97 1.04 5.694 2.77 7.839l-0.0043 0.0034 20.73 25.7 20.73-25.7-0.0043-0.0034c1.73-2.145 2.77-4.869 2.77-7.839z" }
                },
                Reaction::Yes => rsx! {
                    path { d: "M35.236,59.6006h-9 c-8.321,0-14-6.681-14-15c0.0204-4.7991,1.7993-9.4241,5-13" }
                    path { d: "M34.625,31.6006h-4.389 l7-18c0.811-2.084-2.79-3.8-5-1l-15,19" }
                    path { d: "M37.448,35.1006 c0.1527,2.0638,1.9342,3.6226,4,3.5h13c1.933,0.2578,3.709-1.1001,3.9669-3.0331s-1.1001-3.709-3.0331-3.9669 c-0.3099-0.0413-0.6238-0.0413-0.9337,0h-13C39.3822,31.478,37.6007,33.0368,37.448,35.1006z" }
                    path { d: "M38.305,56.4366 c0.1527,2.0638,1.9342,3.6226,4,3.5h7c1.933,0.2578,3.709-1.1001,3.9669-3.0331s-1.1001-3.709-3.0331-3.9669 c-0.3099-0.0413-0.6238-0.0413-0.9337,0h-7C40.2392,52.814,38.4577,54.3728,38.305,56.4366z" }
                    path { d: "M33.236,42.4366 c0.1527,2.0638,1.9342,3.6226,4,3.5h19c1.933,0.2578,3.709-1.1001,3.9669-3.0331s-1.1001-3.709-3.0331-3.9669 c-0.3099-0.0413-0.6238-0.0413-0.9337,0h-19C35.1702,38.814,33.3887,40.3728,33.236,42.4366z" }
                    path { d: "M35.236,49.4366 c0.1527,2.0638,1.9342,3.6226,4,3.5h15c1.933,0.2578,3.709-1.1001,3.9669-3.0331s-1.1001-3.709-3.0331-3.9669 c-0.3099-0.0413-0.6238-0.0413-0.9337,0h-15C37.1702,45.814,35.3887,47.3728,35.236,49.4366z" }
                    path { d: "M14.136,36.2626 c-1.176,2.4511-1.8253,5.1215-1.906,7.839c0,8.56,4.625,15.5,15.125,15.5" }
                },
                Reaction::Down => rsx! {
                    path { d: "M17.236,40.3161 c-3.2007-3.5759-4.9796-8.2009-5-13c0-8.319,5.679-15,14-15h9" }
                    path { d: "M17.236,40.3161l15,19 c2.21,2.8,5.811,1.084,5-1l-7-18h4.389" }
                    path { d: "M41.448,40.3161h13 c0.3099,0.0413,0.6238,0.0413,0.9337,0c1.933-0.2578,3.291-2.0339,3.0331-3.9669s-2.0339-3.291-3.9669-3.0331h-13 c-2.0658-0.1226-3.8473,1.4362-4,3.5C37.6007,38.8799,39.3822,40.4387,41.448,40.3161z" }
                    path { d: "M42.305,18.9801h7 c0.3099,0.0413,0.6238,0.0413,0.9337,0c1.933-0.2578,3.291-2.0339,3.0331-3.9669s-2.0339-3.291-3.9669-3.0331h-7 c-2.0658-0.1226-3.8473,1.4362-4,3.5C38.4577,17.5439,40.2392,19.1027,42.305,18.9801z" }
                    path { d: "M37.236,32.9801h19 c0.3099,0.0413,0.6238,0.0413,0.9337,0c1.933-0.2578,3.291-2.0339,3.0331-3.9669s-2.0339-3.291-3.9669-3.0331h-19 c-2.0658-0.1226-3.8473,1.4362-4,3.5C33.3887,31.5439,35.1702,33.1027,37.236,32.9801z" }
                    path { d: "M39.236,25.9801h15 c0.3099,0.0413,0.6238,0.0413,0.9337,0c1.933-0.2578,3.291-2.0339,3.0331-3.9669s-2.0339-3.291-3.9669-3.0331h-15 c-2.0658-0.1226-3.8473,1.4362-4,3.5C35.3887,24.5439,37.1702,26.1027,39.236,25.9801z" }
                    path { d: "M27.355,12.315c-10.5,0-15.125,6.94-15.125,15.5 c0.0807,2.7175,0.73,5.3879,1.906,7.839" }
                },
                Reaction::Ha => rsx! {
                    path { d: "M58.8479,33.3406q-.01-.0839-.02-.1675A23.0031,23.0031,0,0,0,13.2041,32.92c-.0148.1069-.0386.3127-.052.42" }
                    path { d: "M17.1832,49.229A23.5314,23.5314,0,0,0,36,59a23.3428,23.3428,0,0,0,18.7572-9.6871" }
                    path { d: "M20.6948,28.2a6.3062,6.3062,0,0,1,3.3907-3,6.3038,6.3038,0,0,1,4.53-.4209" }
                    path { d: "M51.3052,28.2A7.19,7.19,0,0,0,43.396,24.77" }
                    path { d: "M50.595,41.64a11.5554,11.5554,0,0,1-.87,4.49c-12.49,3.03-25.43.34-27.49-.13a11.4347,11.4347,0,0,1-.83-4.36h.11s14.8,3.59,28.89.07Z" }
                    path { d: "M49.7251,46.13c-1.79,4.27-6.35,7.23-13.69,7.23-7.41,0-12.03-3.03-13.8-7.36C24.2951,46.47,37.235,49.16,49.7251,46.13Z" }
                    path { d: "M65.7563,47.3378a4.2418,4.2418,0,0,1-6.001.001c-2.9718-2.9717-3.0059-8.961-3.0063-9.0208.2314.0131,6.0508.0625,9.0073,3.019a4.2411,4.2411,0,0,1,0,6.0008Z" }
                    path { d: "M6.2392,47.3385a4.242,4.242,0,0,1-.0011-6.0011c2.9717-2.9717,8.9611-3.0059,9.0208-3.0063-.0131.2314-.0625,6.0509-3.019,9.0074a4.2412,4.2412,0,0,1-6.0007,0Z" }
                    path { d: "M31.6941,35.1642a4.7263,4.7263,0,0,0-8.6382,0" }
                    path { d: "M48.9441,35.1642a4.7263,4.7263,0,0,0-8.6382,0" }
                },
                Reaction::Oh => rsx! {
                    circle { cx: "36.0928", cy: "58.8419", r: "3" }
                    path { d: "M35.9873,49.3729c-1.3809,0-2.5-1.1191-2.5-2.5V11.6636c0-1.3809,1.1191-2.5,2.5-2.5s2.5,1.1191,2.5,2.5v35.2093 C38.4873,48.2538,37.3682,49.3729,35.9873,49.3729z" }
                },
                Reaction::Ask => rsx! {
                    circle { cx: "35.3", cy: "58.84", r: "2.625" }
                    path {
                        fill: "currentColor",
                        stroke: "none",
                        d: "m35.67 10.07c6.57 0 12.03 3.431 12.03 10.21 0 3.671-0.48 6.526-3.71 9.755-3.956 3.956-7.184 6.346-7.208 8.549-0.0651 5.826-0.005 8.278-0.0044 8.36a1.476 1.476 0 0 1-1.464 1.487h-0.0115a1.476 1.476 0 0 1-1.476-1.464c-0.0005 -0.0819-0.0607-2.561 0.0044-8.416 0.0358-3.231 3.63-6.127 8.385-10.88 2.178-2.174 2.474-4.716 2.474-7.127 0-3.282-2.965-7.587-8.984-7.587-5.338 0-8.499 3.467-8.499 9.552a1.434 1.434 0 0 1-1.395 1.587 1.552 1.552 0 0 1-1.505-1.759c0-9.882 7.291-12.27 11.36-12.27m0-2c-4.964 0-13.36 3.005-13.36 14.27a3.519 3.519 0 0 0 3.505 3.759 3.418 3.418 0 0 0 3.395-3.587c0-3.445 1.128-7.552 6.499-7.552 4.817 0 6.984 3.267 6.984 5.587 0 2.364-0.3155 4.144-1.886 5.712-0.7515 0.75-1.474 1.454-2.157 2.12-3.933 3.833-6.775 6.602-6.815 10.15-0.055 4.946-0.0218 7.539-0.005 8.424a3.476 3.476 0 1 0 6.952-0.0248l-0.0012-0.075c-0.0164-0.8595-0.0488-3.384 0.0056-8.248 0.0092-0.8384 2.102-2.822 3.948-4.572 0.8091-0.767 1.726-1.636 2.675-2.585 3.577-3.577 4.296-6.926 4.296-11.17 0-7.301-5.638-12.21-14.03-12.21z",
                    }
                },
            }
        }
    }
}

#[component]
fn PlaceMap(place: Place) -> Element {
    let map = map_window(place);
    let zoom = map.zoom;
    let width = map.width;
    let mut origin = use_signal(|| None::<(f64, f64)>);
    rsx! {
        div {
            class: "place",
            style: "width: {width}px; height: {width}px;",
            role: "button",
            aria_label: "Open this place in Maps",
            onpointerdown: move |event| {
                event.stop_propagation();
                let point = event.client_coordinates();
                origin.set(Some((point.x, point.y)));
            },
            onpointerup: move |event| {
                event.stop_propagation();
                let Some((start_x, start_y)) = origin() else {
                    return;
                };
                origin.set(None);
                let point = event.client_coordinates();
                let dx = point.x - start_x;
                let dy = point.y - start_y;
                if dx * dx + dy * dy < 18.0 * 18.0 {
                    location::open_place(place);
                }
            },
            onpointercancel: move |_| origin.set(None),
            for tile in map.tiles {
                img {
                    class: "tile",
                    alt: "",
                    src: "https://tile.openstreetmap.org/{zoom}/{tile.x}/{tile.y}.png",
                    style: "left: {tile.left}px; top: {tile.top}px; width: {tile.edge}px; height: {tile.edge}px;",
                }
            }
            span { class: "pin" }
        }
        p { class: "map-credit", "© OpenStreetMap" }
    }
}

fn release_note(mut drag: Signal<Option<(f64, f64, f64)>>, mut reveal: Signal<f64>) {
    if drag().is_none() {
        return;
    }
    drag.set(None);
    let next = if reveal() > REVEAL / 2.0 { REVEAL } else { 0.0 };
    reveal.set(next);
}

fn note_class(kind: NoteKind) -> &'static str {
    match kind {
        NoteKind::Text => "",
        NoteKind::Voice => "voice",
        NoteKind::Video => "video",
    }
}

#[cfg(test)]
mod font_tests {
    use super::{base64, paper_css};

    #[test]
    fn the_journal_carries_its_own_faces() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        let css = paper_css();
        assert!(css.contains("font-family: \"Kalam\""));
        assert!(css.contains("font-family: \"Patrick Hand\""));
        assert!(css.contains("font-family: \"Libre Baskerville\""));
        assert!(css.contains("data:font/woff2;base64,"));
        assert!(!css.contains("fonts.googleapis.com"));
    }
}

#[cfg(test)]
mod copy_tests {
    use super::copied_text;

    #[test]
    fn a_copy_uses_the_highlighted_portion_when_there_is_one() {
        assert_eq!(
            copied_text("The pears are ready.", ""),
            "The pears are ready."
        );
        assert_eq!(
            copied_text("The pears are ready.", "   "),
            "The pears are ready."
        );
        assert_eq!(copied_text("The pears are ready.", " pears "), "pears");
    }
}
