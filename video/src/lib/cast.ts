// asciicast v2: a JSON header line, then one [time, kind, data] event per
// line. "o" is terminal output, "i" is what the person typed.

export type CastEvent = { t: number; kind: "o" | "i"; data: string };

export type Cast = {
  command: string;
  width: number;
  title: string;
  events: CastEvent[];
};

export const parseCast = (text: string): Cast => {
  const [head, ...lines] = text.trim().split("\n");
  const header = JSON.parse(head);
  if (header.version !== 2) {
    throw new Error(`asciicast v2 expected, got version ${header.version}`);
  }
  const events = lines.map((line): CastEvent => {
    const [t, kind, data] = JSON.parse(line);
    if (kind !== "o" && kind !== "i") {
      throw new Error(`unexpected asciicast event ${kind}`);
    }
    return { t, kind, data };
  });
  return {
    command: header.command,
    width: header.width,
    title: header.title ?? "",
    events,
  };
};
