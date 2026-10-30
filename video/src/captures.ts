import type { AgentCapture, Captures } from "./lib/timeline";
import { parseCast } from "./lib/cast";
import agentApprove from "../captures/agent-approve.cast";
import agentMcp from "../captures/agent-mcp.json";
import alkanesBalance from "../captures/alkanes-balance.cast";
import alkanesSend from "../captures/alkanes-send.cast";
import assistantApproveSend from "../captures/assistant-approve-send.cast";
import assistantApproveWrap from "../captures/assistant-approve-wrap.cast";
import assistantMcp from "../captures/assistant-mcp.json";

export const captures: Captures = {
  casts: {
    "agent-approve": parseCast(agentApprove),
    "alkanes-balance": parseCast(alkanesBalance),
    "alkanes-send": parseCast(alkanesSend),
    "assistant-approve-send": parseCast(assistantApproveSend),
    "assistant-approve-wrap": parseCast(assistantApproveWrap),
  },
  agents: {
    "agent-mcp": agentMcp as unknown as AgentCapture,
    "assistant-mcp": assistantMcp as unknown as AgentCapture,
  },
};
