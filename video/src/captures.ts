import type { AgentCapture, Captures } from "./lib/timeline";
import { parseCast } from "./lib/cast";
import agentApprove from "../captures/agent-approve.cast";
import agentMcp from "../captures/agent-mcp.json";
import alkanesBalance from "../captures/alkanes-balance.cast";
import alkanesSend from "../captures/alkanes-send.cast";

export const captures: Captures = {
  casts: {
    "agent-approve": parseCast(agentApprove),
    "alkanes-balance": parseCast(alkanesBalance),
    "alkanes-send": parseCast(alkanesSend),
  },
  agents: { "agent-mcp": agentMcp as unknown as AgentCapture },
};
