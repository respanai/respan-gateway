// The client APIs the image playground can speak. Each adapter says how a
// request is written and its images read in its API; the page draws them
// the same way.

import type { ApiOption, Json } from "@/lib/playground-apis";

/** What the page asks for: a size of OpenAI's, `WxH`. */
export interface ImageAsk {
  model: string;
  prompt: string;
  size: string;
  quality: string;
  n: number;
}

/** An image drawn: where it is, and the prompt it was drawn from if rewritten. */
export interface Drawn {
  src: string;
  revised?: string;
}

export interface ImageApi extends ApiOption {
  /** Whether it takes a quality and a count of images. */
  tunes: boolean;
  /** The request that draws `ask`. */
  create: (ask: ImageAsk) => { path: string; body: unknown };
  /** The images of an answer. */
  read: (answer: Json) => Drawn[];
}

const openaiImages: ImageApi = {
  id: "openai-images",
  label: "OpenAI Images",
  endpoint: "/v1/images/generations",
  tunes: true,
  create: ({ model, prompt, size, quality, n }) => ({
    path: "/v1/images/generations",
    body: { model, prompt, size, quality, n },
  }),
  read: (answer) =>
    (answer.data ?? []).map((img: Json) => ({
      src: img.b64_json ? `data:image/png;base64,${img.b64_json}` : (img.url ?? ""),
      revised: img.revised_prompt,
    })),
};

/** Gemini's aspect ratio of each size the page offers. */
const aspects: Record<string, string> = {
  "1024x1024": "1:1",
  "1536x1024": "3:2",
  "1024x1536": "2:3",
};

/**
 * Gemini's image models through `generateContent`: one turn of the prompt,
 * the size an aspect ratio, the images the answer's inline parts. Gemini
 * draws one image, of no quality it is told.
 */
const geminiImages: ImageApi = {
  id: "gemini-images",
  label: "Gemini Images",
  endpoint: "/v1beta/models/{model}:generateContent",
  tunes: false,
  create: ({ model, prompt, size }) => ({
    path: `/v1beta/models/${encodeURIComponent(model)}:generateContent`,
    body: {
      contents: [{ role: "user", parts: [{ text: prompt }] }],
      generationConfig: { responseModalities: ["IMAGE"], imageConfig: { aspectRatio: aspects[size] ?? "1:1" } },
    },
  }),
  read: (answer) =>
    (answer.candidates ?? []).flatMap((c: Json) =>
      (c.content?.parts ?? []).flatMap((part: Json): Drawn[] => {
        if (part.thought) return [];
        if (part.inlineData) return [{ src: `data:${part.inlineData.mimeType};base64,${part.inlineData.data}` }];
        if (part.fileData) return [{ src: part.fileData.fileUri }];
        return [];
      }),
    ),
};

export const imageApis: ImageApi[] = [openaiImages, geminiImages];
