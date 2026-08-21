"use client";

import { useRef, useState } from "react";

export default function CopyButton({ text }: { text: string }) {
  const [label, setLabel] = useState("copy");
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  function copy() {
    const done = (ok: boolean) => {
      setLabel(ok ? "copied" : "failed");
      clearTimeout(timer.current);
      timer.current = setTimeout(() => setLabel("copy"), 1600);
    };
    if (navigator.clipboard?.writeText) {
      navigator.clipboard.writeText(text).then(
        () => done(true),
        () => done(false)
      );
    } else {
      done(false);
    }
  }

  return (
    <button type="button" onClick={copy} aria-label="Copy install command">
      {label}
    </button>
  );
}
