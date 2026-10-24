import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { json, renderApp, serve } from "@/test/render";
import { ChatTab } from "./chat";
import { EmbeddingsTab } from "./embeddings";
import { ImageTab } from "./image";
import { RerankTab } from "./rerank";
import { SpeechTab } from "./speech";
import { TranscribeTab } from "./transcribe";

const models = () => json({ object: "list", data: [] });
const sse = (...chunks: unknown[]) =>
  new Response(
    [...chunks.map((c) => `data: ${JSON.stringify(c)}\n\n`), "data: [DONE]\n\n"].join(""),
    { headers: { "content-type": "text/event-stream" } },
  );
const sentJson = (calls: { key: string; init: RequestInit }[], key: string) =>
  JSON.parse(String(calls.find((c) => c.key === key)?.init.body));

beforeEach(() => {
  URL.createObjectURL = vi.fn(() => "blob:media");
  URL.revokeObjectURL = vi.fn();
});

describe("ChatTab", () => {
  it("streams a reply into the conversation with its usage", async () => {
    const calls = serve({
      "GET /v1/models": models,
      "POST /v1/chat/completions": () =>
        sse(
          { choices: [{ delta: { content: "Hello " } }] },
          { choices: [{ delta: { content: "there" }, finish_reason: "stop" }] },
          { choices: [], usage: { prompt_tokens: 7, completion_tokens: 2 } },
        ),
    });
    renderApp(<ChatTab />);
    await userEvent.type(screen.getByLabelText("Message"), "Hi{Enter}");
    expect(await screen.findByText("Hello there")).toBeInTheDocument();
    expect(await screen.findByText(/7 → 2 tokens/)).toBeInTheDocument();
    expect(screen.getByText("Hi")).toBeInTheDocument();
    expect(sentJson(calls, "POST /v1/chat/completions")).toMatchObject({
      model: "gpt-6-luna",
      stream: true,
      messages: [{ role: "user", content: "Hi" }],
    });
  });

  it("shows a stream that fails midway as an error under the partial reply", async () => {
    serve({
      "GET /v1/models": models,
      "POST /v1/chat/completions": () =>
        sse({ choices: [{ delta: { content: "Par" } }] }, { error: { message: "upstream broke" } }),
    });
    renderApp(<ChatTab />);
    await userEvent.type(screen.getByLabelText("Message"), "Hi{Enter}");
    expect(await screen.findByRole("alert")).toHaveTextContent("upstream broke");
  });
});

describe("ImageTab", () => {
  it("sends the prompt and shows each returned image", async () => {
    const calls = serve({
      "GET /v1/models": models,
      "POST /v1/images/generations": () =>
        json({ data: [{ b64_json: "AAAA", revised_prompt: "A lighthouse, softer" }] }),
    });
    renderApp(<ImageTab />);
    await userEvent.type(screen.getByLabelText("Prompt"), "A lighthouse");
    await userEvent.click(screen.getByRole("button", { name: "Generate" }));
    const image = await screen.findByRole("img", { name: "A lighthouse, softer" });
    expect(image).toHaveAttribute("src", "data:image/png;base64,AAAA");
    expect(sentJson(calls, "POST /v1/images/generations")).toMatchObject({
      model: "gpt-image-1",
      prompt: "A lighthouse",
      size: "1024x1024",
      n: 1,
    });
  });
});

describe("TranscribeTab", () => {
  it("uploads the chosen audio as multipart and shows the transcript", async () => {
    const calls = serve({
      "GET /v1/models": models,
      "POST /v1/audio/transcriptions": () => json({ text: "hello world" }),
    });
    renderApp(<TranscribeTab />);
    const file = new File(["RIFF"], "clip.wav", { type: "audio/wav" });
    await userEvent.upload(document.querySelector<HTMLInputElement>("#transcribe-file")!, file);
    await userEvent.click(screen.getByRole("button", { name: "Transcribe" }));
    expect(await screen.findByText("hello world")).toBeInTheDocument();
    const form = calls.find((c) => c.key === "POST /v1/audio/transcriptions")?.init.body;
    expect(form).toBeInstanceOf(FormData);
    expect((form as FormData).get("model")).toBe("whisper-1");
    expect((form as FormData).get("response_format")).toBe("json");
    expect(((form as FormData).get("file") as File).name).toBe("clip.wav");
  });
});

describe("SpeechTab", () => {
  it("sends the text and offers the audio it gets back", async () => {
    const calls = serve({
      "GET /v1/models": models,
      "POST /v1/audio/speech": () =>
        new Response(new Uint8Array([1, 2, 3]), { headers: { "content-type": "audio/mpeg" } }),
    });
    renderApp(<SpeechTab />);
    await userEvent.type(screen.getByLabelText("Text"), "Hello from the gateway.");
    await userEvent.click(screen.getByRole("button", { name: "Speak" }));
    expect(await screen.findByText("gpt-4o-mini-tts · mp3")).toBeInTheDocument();
    await waitFor(() => expect(screen.getByLabelText("Generated speech")).toBeInTheDocument());
    expect(sentJson(calls, "POST /v1/audio/speech")).toEqual({
      model: "gpt-4o-mini-tts",
      input: "Hello from the gateway.",
      voice: "alloy",
      response_format: "mp3",
    });
  });
});

describe("EmbeddingsTab", () => {
  it("embeds both texts and scores how alike they are", async () => {
    const calls = serve({
      "GET /v1/models": models,
      "POST /v1/embeddings": () =>
        json({
          data: [
            { index: 1, embedding: [0, 1] },
            { index: 0, embedding: [1, 1] },
          ],
        }),
    });
    renderApp(<EmbeddingsTab />);
    await userEvent.type(screen.getByLabelText("Text A"), "a cat");
    await userEvent.type(screen.getByLabelText("Text B"), "a kitten");
    await userEvent.click(screen.getByRole("button", { name: "Embed" }));
    // [1,1] against [0,1] after sorting by index: cos = 1/sqrt(2).
    expect(await screen.findByText("0.7071")).toBeInTheDocument();
    expect(screen.getAllByText("2 dimensions")).toHaveLength(2);
    expect(sentJson(calls, "POST /v1/embeddings")).toEqual({
      model: "text-embedding-3-small",
      input: ["a cat", "a kitten"],
      encoding_format: "float",
    });
    // Each vector stays captioned with the text it was made of.
    await userEvent.type(screen.getByLabelText("Text A"), " on a mat");
    expect(screen.getByText("a cat", { selector: "p" })).toBeInTheDocument();
    expect(screen.queryByText("a cat on a mat", { selector: "p" })).not.toBeInTheDocument();
  });
});

describe("RerankTab", () => {
  it("sends each line as a document and lists them best first with their scores", async () => {
    const calls = serve({
      "GET /v1/models": models,
      "POST /v2/rerank": () =>
        json({
          results: [
            { index: 1, relevance_score: 0.83 },
            { index: 0, relevance_score: 0.02 },
          ],
          usage: { search_units: 1 },
        }),
    });
    renderApp(<RerankTab />);
    await userEvent.type(screen.getByLabelText("Query"), "capital of France");
    await userEvent.type(screen.getByLabelText("Documents, one per line"), "Bananas are yellow{Enter}{Enter}Paris is the capital");
    await userEvent.click(screen.getByRole("button", { name: "Rerank" }));
    const ranked = await screen.findAllByRole("listitem");
    expect(ranked.map((r) => r.textContent)).toEqual(["#1Paris is the capital0.8300", "#2Bananas are yellow0.0200"]);
    expect(screen.getByText("1 search unit")).toBeInTheDocument();
    expect(sentJson(calls, "POST /v2/rerank")).toEqual({
      model: "rerank-v3.5",
      query: "capital of France",
      documents: ["Bananas are yellow", "Paris is the capital"],
    });
  });
});

describe("every tab", () => {
  it("names the API it speaks and links to its kind's requests", async () => {
    serve({ "GET /v1/models": models });
    const tabs = [
      [<ChatTab key="chat" />, "chat", "OpenAI Chat"],
      [<ImageTab key="image" />, "image", "OpenAI Images"],
      [<TranscribeTab key="transcribe" />, "transcription", "OpenAI Transcriptions"],
      [<SpeechTab key="speech" />, "speech", "OpenAI Speech"],
      [<EmbeddingsTab key="embedding" />, "embedding", "OpenAI Embeddings"],
      [<RerankTab key="rerank" />, "rerank", "Cohere Rerank"],
    ] as const;
    for (const [tab, kind, api] of tabs) {
      const { unmount } = renderApp(tab);
      expect(await screen.findByText(api, { selector: "p" })).toBeInTheDocument();
      expect(screen.getByRole("link", { name: "Requests" })).toHaveAttribute("href", `/requests?kind=${kind}`);
      unmount();
    }
  });
});

describe("a link to the playground", () => {
  it("starts the tab of the kind it names on the model it names", async () => {
    serve({ "GET /v1/models": models });
    renderApp(<ChatTab />, "/playground?tab=chat&kind=chat&model=claude-sonnet-5-5");
    expect(await screen.findByPlaceholderText("Message claude-sonnet-5-5…")).toBeInTheDocument();
  });
});
