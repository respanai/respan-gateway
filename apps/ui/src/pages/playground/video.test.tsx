import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { json, renderApp, serve, type Handler } from "@/test/render";
import { VideoTab } from "./video";

const models = () => json({ object: "list", data: [] });
const poll = (...steps: Handler[]) => {
  let n = 0;
  return (req: Parameters<Handler>[0]) => steps[Math.min(n++, steps.length - 1)](req);
};
const job = (status: string, extra: object = {}) => () => json({ id: "v_1", status, ...extra });
const mp4 = () =>
  new Response(new Uint8Array([1, 2, 3]), { headers: { "content-type": "video/mp4" } });

/** Moves the polling clock; only intervals are faked, so requests settle on their own. */
const advance = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));
const polls = (calls: { key: string }[]) => calls.filter((c) => c.key === "GET /v1/videos/v_1");

/** Types a prompt and presses Create. */
function create() {
  renderApp(<VideoTab />);
  fireEvent.change(screen.getByLabelText("Prompt"), { target: { value: "a paper boat" } });
  fireEvent.click(screen.getByRole("button", { name: "Create video" }));
}

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
  URL.createObjectURL = vi.fn(() => "blob:video");
  URL.revokeObjectURL = vi.fn();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("VideoTab", () => {
  it("creates a job, follows it every 2 s, and plays the file once completed", async () => {
    const calls = serve({
      "GET /v1/models": models,
      "POST /v1/videos": job("queued"),
      "GET /v1/videos/v_1": poll(job("in_progress", { progress: 40 }), job("completed")),
      "GET /v1/videos/v_1/content": mp4,
    });
    create();
    const bar = await screen.findByRole("progressbar", { name: "Video progress" });
    await waitFor(() => expect(bar).toHaveAttribute("aria-valuenow", "40"));
    expect(JSON.parse(String(calls.find((c) => c.key === "POST /v1/videos")?.init.body))).toEqual({
      model: "gemini-omni-1.1-flash",
      prompt: "a paper boat",
      seconds: "4",
    });
    expect(polls(calls)).toHaveLength(1);
    await advance(2000);
    expect(await screen.findByLabelText("Generated video")).toHaveAttribute("src", "blob:video");
    // Done: no more polling.
    await advance(10_000);
    expect(polls(calls)).toHaveLength(2);
    expect(calls.filter((c) => c.key.endsWith("/content"))).toHaveLength(1);
  });

  it("reports a failed job with the provider's message, and stops polling", async () => {
    const calls = serve({
      "GET /v1/models": models,
      "POST /v1/videos": job("queued"),
      "GET /v1/videos/v_1": job("failed", { error: { message: "content policy" } }),
    });
    create();
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("The video couldn't be made");
    expect(alert).toHaveTextContent("content policy");
    await advance(10_000);
    expect(polls(calls)).toHaveLength(1);
    expect(calls.some((c) => c.key.endsWith("/content"))).toBe(false);
  });

  it("says it lost the job when polling fails, and stops asking", async () => {
    const calls = serve({
      "GET /v1/models": models,
      "POST /v1/videos": job("queued"),
      "GET /v1/videos/v_1": () => json({ error: { message: "no such job" } }, 404),
    });
    create();
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Lost track of the job");
    expect(alert).toHaveTextContent("no such job");
    await advance(10_000);
    expect(polls(calls)).toHaveLength(1);
  });

  it("offers a retry of the download when a finished video's file fails", async () => {
    let attempts = 0;
    const calls = serve({
      "GET /v1/models": models,
      "POST /v1/videos": job("queued"),
      "GET /v1/videos/v_1": job("completed"),
      "GET /v1/videos/v_1/content": () =>
        ++attempts === 1 ? json({ error: { message: "storage down" } }, 502) : mp4(),
    });
    create();
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("The video is ready, but its file wouldn't download");
    expect(alert).toHaveTextContent("storage down");
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByLabelText("Generated video")).toBeInTheDocument();
    // Only the file was asked for again, not a new job.
    expect(calls.filter((c) => c.key === "POST /v1/videos")).toHaveLength(1);
    expect(calls.filter((c) => c.key.endsWith("/content"))).toHaveLength(2);
  });

  it("points at the providers page when no provider serves video", async () => {
    serve({
      "GET /v1/models": models,
      "POST /v1/videos": () => json({ error: { message: "no provider for gemini-omni-1.1-flash" } }, 404),
    });
    create();
    expect(await screen.findByText("No provider serves video yet")).toBeInTheDocument();
  });

  it("speaks Gemini Veo: an operation polled by its name, its video at the gateway", async () => {
    const name = "models/gemini-omni-1.1-flash/operations/veo_op1";
    const operation = (extra: object = {}) => () => json({ name, ...extra });
    const uri = "http://gateway.example/v1beta/files/veo_op1:download?alt=media";
    const calls = serve({
      "GET /v1/models": models,
      "GET /ui/api/routes": () =>
        json({
          serve: [
            { format: "openai-videos", kind: "video", endpoints: [] },
            { format: "gemini-veo", kind: "video", endpoints: [] },
          ],
          routes: [],
          defaults: [],
        }),
      "POST /v1beta/models/gemini-omni-1.1-flash:predictLongRunning": operation(),
      [`GET /v1beta/${name}`]: poll(
        operation(),
        operation({ done: true, response: { generateVideoResponse: { generatedSamples: [{ video: { uri } }] } } }),
      ),
      "GET /v1beta/files/veo_op1:download": mp4,
    });
    renderApp(<VideoTab />);
    fireEvent.click(await screen.findByRole("combobox", { name: "API" }));
    fireEvent.click(await screen.findByRole("option", { name: "Gemini Veo" }));
    fireEvent.change(screen.getByLabelText("Prompt"), { target: { value: "a paper boat" } });
    fireEvent.click(screen.getByRole("button", { name: "Create video" }));
    // Veo says no progress: the bar shows none.
    const bar = await screen.findByRole("progressbar", { name: "Video progress" });
    expect(bar).not.toHaveAttribute("aria-valuenow");
    const sent = calls.find((c) => c.key.startsWith("POST /v1beta/"))?.init.body;
    expect(JSON.parse(String(sent))).toEqual({
      instances: [{ prompt: "a paper boat" }],
      parameters: { durationSeconds: 4 },
    });
    await advance(2000);
    expect(await screen.findByLabelText("Generated video")).toHaveAttribute("src", "blob:video");
    // The video came from the gateway, at the path the URI names.
    expect(calls.some((c) => c.key === "GET /v1beta/files/veo_op1:download")).toBe(true);
  });
});
