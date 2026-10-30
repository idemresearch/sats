import React from "react";
import { mix, progress } from "../lib/anim";
import { CARD_COMPACT } from "../lib/layout";
import type { Timeline } from "../lib/timeline";
import { color, ease, mono } from "../theme";

const FONT = 21;

// Long values keep their ends, as `sats agent requests` shortens them.
const middle = (value: string, keep: number) =>
  value.length <= keep * 2 + 1 ? value : `${value.slice(0, keep)}…${value.slice(-keep)}`;

export const Badge: React.FC<{ text: string; tone: string; p: number }> = ({ text, tone, p }) => (
  <span
    style={{
      display: "inline-flex",
      alignItems: "center",
      gap: 10,
      padding: "5px 14px",
      borderRadius: 999,
      border: `1px solid ${tone}`,
      background: `color-mix(in srgb, ${tone} 13%, transparent)`,
      color: tone,
      fontSize: 19,
      lineHeight: 1.2,
      opacity: p,
      transform: `scale(${mix(0.86, 1, p)})`,
      transformOrigin: "left center",
    }}
  >
    <span style={{ width: 8, height: 8, borderRadius: "50%", background: tone }} />
    {text}
  </span>
);

const Row: React.FC<{ label: string; value: string; p: number }> = ({ label, value, p }) => (
  <div
    style={{
      display: "flex",
      gap: 20,
      opacity: p,
      transform: `translateY(${mix(10, 0, p)}px)`,
    }}
  >
    <span style={{ width: 118, color: color.subtle }}>{label}</span>
    <span style={{ color: color.output }}>{value}</span>
  </div>
);

export const AgentCard: React.FC<{ timeline: Timeline; t: number }> = ({ timeline, t }) => {
  const { agent, layout, marks } = timeline;
  if (!agent) return null;
  const [request, check] = agent.capture.calls;
  const filed = request.result as {
    status: string;
    request_id: string;
    message: string;
    asset_transfer: { protocol: string; id: string };
  };
  const observed = check.result as { status: string; txid: string };

  const enter = progress(t, agent.enter, 0.75, ease.out);
  const step = (start: number, i: number) => progress(t, start + i * 0.07, 0.45, ease.out);
  const result = progress(t, agent.result, 0.5, ease.out);
  const update = progress(t, agent.update, 0.5, ease.out);
  // The human copies the request id from here into `sats agent approve`.
  const copied = progress(t, marks["approve.start"] ?? Infinity, 0.3) *
    (1 - progress(t, marks["approve.enter"] ?? Infinity, 0.5));
  const [before, after] = filed.message.split(filed.request_id);

  return (
    <div
      style={{
        position: "absolute",
        left: layout.card.x,
        top: layout.card.y,
        width: layout.card.w,
        height: mix(CARD_COMPACT, layout.cardResult.h, progress(t, agent.update - 0.15, 0.6, ease.inOut)),
        borderRadius: 22,
        border: `1px solid ${color.borderStrong}`,
        background: "#0f0f0f",
        boxShadow: "0 60px 160px rgba(0,0,0,0.7), inset 0 1px 0 rgba(255,255,255,0.05)",
        fontFamily: mono,
        fontSize: FONT,
        lineHeight: "32px",
        opacity: enter,
        transform: `translateX(${mix(70, 0, enter)}px) scale(${mix(0.96, 1, enter)})`,
        overflow: "hidden",
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 14,
          height: 68,
          padding: "0 30px",
          borderBottom: `1px solid ${color.border}`,
          background: "rgba(255,255,255,0.012)",
        }}
      >
        <span
          style={{
            width: 30,
            height: 30,
            borderRadius: 9,
            border: `1px solid ${color.borderStrong}`,
            display: "grid",
            placeItems: "center",
          }}
        >
          <span
            style={{
              width: 10,
              height: 10,
              borderRadius: 2,
              background: color.accent,
              transform: "rotate(45deg)",
            }}
          />
        </span>
        <span style={{ color: color.foreground, fontWeight: 600 }}>{agent.capture.agent}</span>
        <span style={{ color: color.subtle }}>agent</span>
        <span
          style={{
            marginLeft: "auto",
            color: color.subtle,
            fontSize: 16,
            padding: "3px 12px",
            borderRadius: 8,
            border: `1px solid ${color.border}`,
          }}
        >
          MCP
        </span>
      </div>
      <div style={{ padding: "26px 30px", display: "flex", flexDirection: "column", gap: 6 }}>
        <div style={{ color: color.foreground, fontWeight: 600, opacity: step(agent.call, 0) }}>
          <span style={{ color: color.accent }}>→ </span>
          {request.tool}
        </div>
        <Row label="id" value={String(request.arguments.id)} p={step(agent.call, 1)} />
        <Row label="amount" value={String(request.arguments.amount)} p={step(agent.call, 2)} />
        <Row label="address" value={middle(String(request.arguments.address), 14)} p={step(agent.call, 3)} />
        <div style={{ height: 14 }} />
        <div style={{ display: "flex", alignItems: "center", gap: 14, opacity: result > 0 ? 1 : 0 }}>
          <span style={{ color: color.subtle, opacity: result }}>←</span>
          <Badge text={filed.status} tone={color.accent} p={result} />
        </div>
        {/* What sats recorded: a name typed by the agent becomes its id. */}
        <Row
          label="asset"
          value={`${filed.asset_transfer.protocol}:${filed.asset_transfer.id}`}
          p={progress(t, agent.result + 0.12, 0.45, ease.out)}
        />
        <div
          style={{
            color: color.muted,
            fontSize: 19,
            lineHeight: "29px",
            opacity: progress(t, agent.result + 0.18, 0.5, ease.out),
            whiteSpace: "normal",
          }}
        >
          {before}
          <span
            style={{
              color: copied > 0 ? color.foreground : color.muted,
              background: `rgba(247, 147, 26, ${0.22 * copied})`,
              borderRadius: 5,
              boxShadow: `0 0 0 3px rgba(247, 147, 26, ${0.22 * copied})`,
            }}
          >
            {filed.request_id}
          </span>
          {after}
        </div>
        <div style={{ height: 18 }} />
        <div style={{ color: color.foreground, fontWeight: 600, opacity: update }}>
          <span style={{ color: color.accent }}>→ </span>
          {check.tool}
        </div>
        <div style={{ display: "flex", alignItems: "center", gap: 14, opacity: update > 0 ? 1 : 0 }}>
          <span style={{ color: color.subtle, opacity: update }}>←</span>
          <Badge text={observed.status} tone={color.success} p={progress(t, agent.update + 0.12, 0.45, ease.out)} />
          <span style={{ color: color.subtle, fontSize: 19, opacity: progress(t, agent.update + 0.25, 0.45) }}>
            txid {middle(observed.txid, 8)}
          </span>
        </div>
      </div>
    </div>
  );
};
