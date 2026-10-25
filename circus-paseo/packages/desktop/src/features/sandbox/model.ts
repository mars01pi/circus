import { z } from "zod";

const responseSchema = z.object({
  id: z.string().min(1),
  object: z.literal("response"),
  status: z.enum(["completed", "incomplete"]),
  output: z.array(z.unknown()),
  error: z.null().optional(),
});

/** Validate the small non-streaming probe without retaining or displaying its output. */
export async function acceptedResponse(response: Response): Promise<boolean> {
  const reader = response.body?.getReader();
  if (!reader) return false;
  const chunks: Uint8Array[] = [];
  let length = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      length += value.byteLength;
      if (length > 65536) return false;
      chunks.push(value);
    }
    return responseSchema.safeParse(JSON.parse(Buffer.concat(chunks).toString("utf8"))).success;
  } catch {
    return false;
  } finally {
    await reader.cancel();
  }
}
