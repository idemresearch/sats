import React from "react";
import { mix, progress } from "../lib/anim";
import {
  BUBBLE_COLUMNS,
  BUBBLE_MAX,
  BUBBLE_PAD_X,
  BUBBLE_PAD_Y,
  CHAT_CHAR,
  CHAT_FONT,
  CHAT_HEADER,
  CHAT_INPUT,
  CHAT_LINE,
  CHAT_PAD,
  CHIP_GAP,
  CHIP_HEIGHT,
  type ChatItem,
  INNER,
  REPLY_COLUMNS,
  TOOL_COMPACT,
  TOOL_HEAD,
  TOOL_RESULT,
  TOOL_ROW,
  chatLayout,
  middle,
  wrap,
} from "../lib/chat";
import type { Timeline } from "../lib/timeline";
import { color, ease, mono } from "../theme";
import { Badge } from "./AgentCard";

const CARET_BLINK = 1.06;

const tone = (status: string) =>
  status === "sent" ? color.success : status === "denied" || status === "failed" ? color.danger : color.accent;

/** The agent's mark: the diamond the agent card uses. */
const Diamond: React.FC<{ size: number }> = ({ size }) => (
  <span
    style={{
      width: size,
      height: size,
      borderRadius: size * 0.3,
      border: `1px solid ${color.borderStrong}`,
      display: "inline-grid",
      placeItems: "center",
      flex: "none",
    }}
  >
    <span
      style={{
        width: size * 0.34,
        height: size * 0.34,
        borderRadius: 2,
        background: color.accent,
        transform: "rotate(45deg)",
      }}
    />
  </span>
);

// Three dots while a tool runs.
const Working: React.FC<{ t: number; start: number }> = ({ t, start }) => (
  <span style={{ display: "inline-flex", gap: 7 }}>
    {[0, 1, 2].map((i) => (
      <span
        key={i}
        style={{
          width: 7,
          height: 7,
          borderRadius: "50%",
          background: color.subtle,
          opacity: 0.35 + 0.65 * Math.max(0, Math.sin((t - start) * 9 - i * 0.9)),
        }}
      />
    ))}
  </span>
);

const User: React.FC<{ item: ChatItem & { kind: "user" } }> = ({ item }) => {
  const lines = wrap(item.text, BUBBLE_COLUMNS);
  const width = Math.min(BUBBLE_MAX, Math.max(...lines.map((l) => l.length)) * CHAT_CHAR + 2 * BUBBLE_PAD_X);
  return (
    <div
      style={{
        marginLeft: "auto",
        width,
        padding: `${BUBBLE_PAD_Y}px ${BUBBLE_PAD_X}px`,
        borderRadius: "20px 20px 6px 20px",
        background: "#1d1d1d",
        border: `1px solid ${color.border}`,
        color: color.foreground,
        boxSizing: "border-box",
        whiteSpace: "pre",
      }}
    >
      {lines.join("\n")}
    </div>
  );
};

const Tool: React.FC<{ item: ChatItem & { kind: "tool" }; t: number }> = ({ item, t }) => {
  const status = String(item.call.result.status);
  const txid = item.call.result.txid;
  const done = progress(t, item.result, 0.45, ease.out);
  const answer = (
    <span style={{ display: "inline-flex", alignItems: "center", gap: 14 }}>
      {t < item.result ? (
        <Working t={t} start={item.t} />
      ) : (
        <>
          <span style={{ color: color.subtle, opacity: done }}>←</span>
          <Badge text={status} tone={tone(status)} p={done} />
          {typeof txid === "string" ? (
            <span style={{ color: color.subtle, fontSize: 19, opacity: progress(t, item.result + 0.15, 0.4) }}>
              txid {middle(txid, 4)}
            </span>
          ) : null}
        </>
      )}
    </span>
  );
  const compact = item.rows.length === 0;
  return (
    <div
      style={{
        borderRadius: 16,
        border: `1px solid ${color.borderStrong}`,
        background: "#121212",
        overflow: "hidden",
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 12,
          height: compact ? TOOL_COMPACT - 2 : TOOL_HEAD,
          padding: "0 22px",
          borderBottom: compact ? undefined : `1px solid ${color.border}`,
          fontSize: 21,
        }}
      >
        <span style={{ color: color.subtle }}>sats</span>
        <span style={{ color: color.borderStrong }}>/</span>
        <span style={{ color: color.foreground, fontWeight: 600 }}>{item.call.tool}</span>
        {compact ? <span style={{ marginLeft: "auto" }}>{answer}</span> : null}
      </div>
      {compact ? null : (
        <div style={{ padding: "8px 22px 14px" }}>
          {item.rows.map(([label, value], i) => {
            const p = progress(t, item.t + 0.2 + i * 0.08, 0.4, ease.out);
            return (
              <div
                key={label}
                style={{
                  display: "flex",
                  height: TOOL_ROW,
                  alignItems: "center",
                  fontSize: 21,
                  opacity: p,
                  transform: `translateY(${mix(8, 0, p)}px)`,
                }}
              >
                <span style={{ width: 170, color: color.subtle }}>{label}</span>
                <span style={{ color: color.output }}>{value}</span>
              </div>
            );
          })}
          <div style={{ height: TOOL_RESULT, display: "flex", alignItems: "flex-end" }}>{answer}</div>
        </div>
      )}
    </div>
  );
};

const Reply: React.FC<{ item: ChatItem & { kind: "reply" }; t: number }> = ({ item, t }) => {
  let word = 0;
  const lines = wrap(item.text, REPLY_COLUMNS).map((line) =>
    line.split(" ").map((text) => ({ text, at: item.words[word++] })),
  );
  const command = item.command;
  const chip = command ? progress(t, command.t, 0.45, ease.out) : 0;
  const copied = command?.copied
    ? progress(t, command.copied[0], 0.3) * (1 - progress(t, command.copied[1], 0.5))
    : 0;
  return (
    <div style={{ color: color.foreground }}>
      {lines.map((words, i) => (
        <div key={i} style={{ height: CHAT_LINE, whiteSpace: "pre" }}>
          {words.map(({ text, at }, j) => (
            <span key={j} style={{ opacity: progress(t, at, 0.2) }}>
              {j ? " " : ""}
              {text}
            </span>
          ))}
        </div>
      ))}
      {command ? (
        <div
          style={{
            marginTop: CHIP_GAP,
            height: CHIP_HEIGHT,
            width: (command.text.length + 2) * CHAT_CHAR + 44,
            display: "flex",
            alignItems: "center",
            gap: CHAT_CHAR,
            padding: "0 22px",
            boxSizing: "border-box",
            borderRadius: 12,
            border: `1px solid ${copied > 0 ? `rgba(247, 147, 26, ${0.3 + 0.5 * copied})` : color.border}`,
            background: copied > 0 ? `rgba(247, 147, 26, ${0.12 * copied})` : "#161616",
            boxShadow: `0 0 ${28 * copied}px rgba(247, 147, 26, ${0.25 * copied})`,
            opacity: chip,
            transform: `translateY(${mix(10, 0, chip)}px)`,
            whiteSpace: "pre",
          }}
        >
          <span style={{ color: color.accent, fontWeight: 700 }}>$</span>
          <span style={{ fontWeight: 600 }}>{command.text}</span>
        </div>
      ) : null}
    </div>
  );
};

/** The assistant: a chat with a person, its MCP calls to sats shown as they run. */
export const Chat: React.FC<{ timeline: Timeline; t: number; rise: number }> = ({ timeline, t, rise }) => {
  const { chat, layout, launch } = timeline;
  const rect = layout.chat;
  if (!chat || !rect || !launch.chat) return null;
  const { tops, offset, area } = chatLayout(chat.items, rect.h, t);

  // What the person is typing, until they send it.
  const typing = chat.items.find(
    (item): item is ChatItem & { kind: "user" } => item.kind === "user" && item.keys[0] - 0.8 <= t && t < item.t,
  );
  const typed = typing ? typing.text.slice(0, typing.keys.filter((k) => k <= t).length) : "";
  const lastKey = typing ? Math.max(-Infinity, ...typing.keys.filter((k) => k <= t)) : -Infinity;
  const caret = typing && (t - lastKey < 0.45 || (t - Math.max(lastKey, 0)) % CARET_BLINK < CARET_BLINK / 2);
  const sent = chat.items.find((item) => item.kind === "user" && t >= item.t && t < item.t + 0.35);
  const pressed = sent ? 1 - progress(t, sent.t, 0.35) : 0;

  return (
    <div
      style={{
        position: "absolute",
        left: rect.x,
        top: rect.y,
        width: rect.w,
        height: rect.h,
        borderRadius: 22,
        border: `1px solid ${color.borderStrong}`,
        background: "#0f0f0f",
        boxShadow: "0 60px 160px rgba(0,0,0,0.7), inset 0 1px 0 rgba(255,255,255,0.05)",
        fontFamily: mono,
        fontSize: CHAT_FONT,
        lineHeight: `${CHAT_LINE}px`,
        overflow: "hidden",
        opacity: rise,
        transformOrigin: "50% 100%",
        transform: `perspective(2600px) translateY(${mix(260, 0, rise)}px) rotateX(${mix(20, 0, rise)}deg) scale(${mix(
          0.92,
          1,
          rise,
        )})`,
        fontVariantLigatures: "none",
        fontFeatureSettings: '"liga" 0, "calt" 0',
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 14,
          height: CHAT_HEADER,
          padding: `0 ${CHAT_PAD - 6}px`,
          borderBottom: `1px solid ${color.border}`,
          background: "rgba(255,255,255,0.012)",
          boxSizing: "border-box",
        }}
      >
        <Diamond size={30} />
        <span style={{ color: color.foreground, fontWeight: 600, fontSize: 21 }}>{launch.chat.name}</span>
        <span style={{ color: color.subtle, fontSize: 21 }}>{launch.chat.label}</span>
        <span
          style={{
            marginLeft: "auto",
            display: "flex",
            alignItems: "center",
            gap: 9,
            color: color.subtle,
            fontSize: 16,
            padding: "3px 12px",
            borderRadius: 8,
            border: `1px solid ${color.border}`,
          }}
        >
          <span style={{ width: 7, height: 7, borderRadius: "50%", background: color.success }} />
          sats · MCP
        </span>
      </div>
      <div
        style={{
          position: "absolute",
          left: 0,
          right: 0,
          top: CHAT_HEADER,
          height: area,
          overflow: "hidden",
          maskImage: "linear-gradient(transparent, black 26px, black calc(100% - 18px), transparent)",
        }}
      >
        {chat.items.map((item, i) => {
          if (t < item.t) return null;
          const p = progress(t, item.t, 0.45, ease.out);
          return (
            <div
              key={item.id}
              style={{
                position: "absolute",
                left: CHAT_PAD,
                width: INNER,
                top: tops[i] - offset,
                opacity: p,
                transform: `translateY(${mix(16, 0, p)}px)`,
              }}
            >
              {item.kind === "user" ? (
                <User item={item} />
              ) : item.kind === "tool" ? (
                <Tool item={item} t={t} />
              ) : (
                <Reply item={item} t={t} />
              )}
            </div>
          );
        })}
      </div>
      <div
        style={{
          position: "absolute",
          left: 24,
          right: 24,
          bottom: 26,
          height: CHAT_INPUT - 52,
          display: "flex",
          alignItems: "center",
          gap: 16,
          padding: "0 14px 0 24px",
          borderRadius: 18,
          border: `1px solid ${typing ? color.borderStrong : color.border}`,
          background: "#151515",
          boxSizing: "border-box",
        }}
      >
        <span style={{ flex: 1, whiteSpace: "pre", color: typed ? color.foreground : color.subtle }}>
          {typed || launch.chat.placeholder}
          {caret ? (
            <span
              style={{
                display: "inline-block",
                width: 2,
                height: CHAT_FONT * 1.15,
                marginLeft: typed ? 2 : -CHAT_CHAR * launch.chat.placeholder.length,
                verticalAlign: "-0.22em",
                background: color.accent,
              }}
            />
          ) : null}
        </span>
        <span
          style={{
            width: 44,
            height: 44,
            borderRadius: 12,
            display: "grid",
            placeItems: "center",
            background: typed ? color.foreground : color.surface2,
            transform: `scale(${1 - 0.12 * pressed})`,
          }}
        >
          <svg width="20" height="20" viewBox="0 0 20 20">
            <path
              d="M10 15.5 V4.5 M5 9.5 L10 4.5 L15 9.5"
              fill="none"
              stroke={typed ? color.background : color.subtle}
              strokeWidth={2.2}
              strokeLinecap="round"
              strokeLinejoin="round"
            />
          </svg>
        </span>
      </div>
    </div>
  );
};
