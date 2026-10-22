import { describe, expect, it } from "vitest";
import { imageApis } from "./image-apis";

const api = (id: string) => imageApis.find((a) => a.id === id)!;
const ask = { model: "gpt-image-2", prompt: "a dot", size: "1536x1024", quality: "low", n: 2 };

describe("image APIs", () => {
  it("asks Gemini for one turn of the prompt, its size an aspect ratio", () => {
    const { path, body } = api("gemini-images").create(ask);
    expect(path).toBe("/v1beta/models/gpt-image-2:generateContent");
    expect(body).toEqual({
      contents: [{ role: "user", parts: [{ text: "a dot" }] }],
      generationConfig: { responseModalities: ["IMAGE"], imageConfig: { aspectRatio: "3:2" } },
    });
  });

  it("reads Gemini's inline images, not its thoughts or text", () => {
    const answer = {
      candidates: [
        {
          content: {
            parts: [
              { text: "here", thought: true },
              { inlineData: { mimeType: "image/jpeg", data: "YQ==" }, thought: true },
              { text: "here it is" },
              { inlineData: { mimeType: "image/jpeg", data: "Yg==" } },
            ],
          },
        },
      ],
    };
    expect(api("gemini-images").read(answer)).toEqual([{ src: "data:image/jpeg;base64,Yg==" }]);
  });

  it("asks OpenAI for its quality and count, and reads its revised prompts", () => {
    expect(api("openai-images").create(ask).body).toEqual({
      model: "gpt-image-2",
      prompt: "a dot",
      size: "1536x1024",
      quality: "low",
      n: 2,
    });
    const read = api("openai-images").read({ data: [{ b64_json: "YQ==", revised_prompt: "a red dot" }] });
    expect(read).toEqual([{ src: "data:image/png;base64,YQ==", revised: "a red dot" }]);
  });
});
