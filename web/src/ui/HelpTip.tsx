// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Fusion-style rich hover card: rest the pointer on (or keyboard-focus) a
// control and a card with a title + explanation appears beside it. Content is
// plain text today; HelpContent carries an optional illustration slot so
// detailed picture instructions can be added later without touching this
// component.

import {
  cloneElement,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactElement,
  type SyntheticEvent,
} from "react";
import { createPortal } from "react-dom";

export interface HelpContent {
  title: string;
  /** Explanation paragraphs (plain text). */
  text: string[];
  /** Optional illustration (static asset URL) shown under the text. */
  img?: string;
  imgAlt?: string;
}

const SHOW_DELAY_MS = 400;

/** Wraps a single element (typically a button) without adding DOM around it —
 *  the child is cloned with hover/focus handlers, so grid/flex layouts that
 *  size direct children are unaffected. */
export function HelpTip({
  help,
  children,
}: {
  help: HelpContent;
  children: ReactElement<Record<string, unknown>>;
}) {
  const [anchor, setAnchor] = useState<DOMRect | null>(null);
  const timer = useRef(0);
  useEffect(() => () => window.clearTimeout(timer.current), []);

  const show = (e: SyntheticEvent) => {
    const el = e.currentTarget as HTMLElement;
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setAnchor(el.getBoundingClientRect()), SHOW_DELAY_MS);
  };
  const hide = () => {
    window.clearTimeout(timer.current);
    setAnchor(null);
  };
  const chain =
    (name: string, next: (e: SyntheticEvent) => void) =>
    (e: SyntheticEvent, ...rest: unknown[]) => {
      (children.props[name] as ((...a: unknown[]) => void) | undefined)?.(e, ...rest);
      next(e);
    };

  const trigger = cloneElement(children, {
    onMouseEnter: chain("onMouseEnter", show),
    onMouseLeave: chain("onMouseLeave", hide),
    onFocus: chain("onFocus", show),
    onBlur: chain("onBlur", hide),
    // The action happened — get the card out of the way.
    onClick: chain("onClick", hide),
  });

  return (
    <>
      {trigger}
      {anchor && createPortal(<HelpCard help={help} anchor={anchor} />, document.body)}
    </>
  );
}

/** Global replacement for native `title` tooltips, so every hover hint in the
 *  app shares the help-card look. Mounted once in App. On hover it stashes the
 *  element's `title` into `data-tip` (suppressing the browser tooltip) and
 *  shows a compact styled card below the element instead. Call sites keep
 *  writing plain `title="…"` — including future ones. */
export function TitleTipLayer() {
  const [tip, setTip] = useState<{ text: string; rect: DOMRect } | null>(null);
  const timer = useRef(0);
  const cur = useRef<HTMLElement | null>(null);

  useEffect(() => {
    const hide = () => {
      window.clearTimeout(timer.current);
      cur.current = null;
      setTip(null);
    };
    const leave = (e: Event) => {
      if (e.target === cur.current) hide();
    };
    const over = (e: PointerEvent) => {
      const el = (e.target as Element | null)?.closest?.("[title], [data-tip]") as
        | HTMLElement
        | null;
      if (el === cur.current) return;
      if (cur.current) hide();
      if (!el) return;
      if (el.hasAttribute("title")) {
        const t = el.getAttribute("title") ?? "";
        el.removeAttribute("title");
        if (t.trim()) {
          el.setAttribute("data-tip", t);
          // Icon-only controls named by their title keep an accessible name.
          if (!el.hasAttribute("aria-label") && !(el.textContent ?? "").trim())
            el.setAttribute("aria-label", t);
        }
      }
      const text = el.getAttribute("data-tip");
      if (!text) return;
      cur.current = el;
      el.addEventListener("pointerleave", leave, { once: true });
      window.clearTimeout(timer.current);
      timer.current = window.setTimeout(
        () => setTip({ text, rect: el.getBoundingClientRect() }),
        SHOW_DELAY_MS
      );
    };
    const down = () => hide();
    document.addEventListener("pointerover", over, true);
    document.addEventListener("pointerdown", down, true);
    document.addEventListener("wheel", down, { capture: true, passive: true });
    window.addEventListener("blur", down);
    return () => {
      document.removeEventListener("pointerover", over, true);
      document.removeEventListener("pointerdown", down, true);
      document.removeEventListener("wheel", down, true);
      window.removeEventListener("blur", down);
      window.clearTimeout(timer.current);
    };
  }, []);

  return tip ? createPortal(<TipCard text={tip.text} anchor={tip.rect} />, document.body) : null;
}

/** Compact text-only card for `title` hints: below the element, centered,
 *  clamped to the viewport, flipping above when there's no room. */
function TipCard({ text, anchor }: { text: string; anchor: DOMRect }) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    let left = anchor.left + anchor.width / 2 - el.offsetWidth / 2;
    left = Math.min(Math.max(8, left), vw - el.offsetWidth - 8);
    let top = anchor.bottom + 8;
    if (top + el.offsetHeight > vh - 8) top = Math.max(8, anchor.top - 8 - el.offsetHeight);
    setPos({ left, top });
  }, [anchor, text]);
  return (
    <div ref={ref} className="helpcard tip" role="tooltip" style={pos ?? { left: -9999, top: 0 }}>
      {text}
    </div>
  );
}

/** The floating card. Prefers the right side of the anchor (the step panel
 *  sits at the left edge), flips left when there's no room, clamps vertically.
 *  pointer-events: none in CSS — it can never trap the mouse. */
function HelpCard({ help, anchor }: { help: HelpContent; anchor: DOMRect }) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    let left = anchor.right + 12;
    if (left + el.offsetWidth > vw - 8) left = Math.max(8, anchor.left - 12 - el.offsetWidth);
    const top = Math.min(Math.max(8, anchor.top - 8), Math.max(8, vh - el.offsetHeight - 8));
    setPos({ left, top });
  }, [anchor]);
  return (
    <div
      ref={ref}
      className="helpcard"
      role="tooltip"
      style={pos ?? { left: -9999, top: 0 }}
    >
      <b>{help.title}</b>
      {help.text.map((t, i) => (
        <p key={i}>{t}</p>
      ))}
      {help.img && <img src={help.img} alt={help.imgAlt ?? ""} />}
    </div>
  );
}
