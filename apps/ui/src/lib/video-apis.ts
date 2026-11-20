// The client APIs the video playground can speak. Each adapter says how a
// job is created, read, polled and downloaded in its API; the page reads
// every job the same way.

import type { ApiOption, Json } from "@/lib/playground-apis";

/** A video job, as the page reads it whatever the API. */
export interface VideoJob {
  /** What the job is polled by: OpenAI's id, or Gemini's operation name. */
  id: string;
  status: "queued" | "in_progress" | "completed" | "failed";
  /** In percent; none from an API that says none, as Gemini Omni. */
  progress?: number;
  error?: string;
}

export interface VideoApi extends ApiOption {
  /** The request that creates a job of `model`. */
  create: (model: string, prompt: string, seconds: string) => { path: string; body: unknown };
  /** A job, from an answer to its creation or a poll. */
  read: (answer: Json) => VideoJob;
  /** Where `job` is polled. */
  poll: (job: VideoJob, answer: Json) => string;
  /** Where a finished job's video is. */
  content: (job: VideoJob, answer: Json) => string;
}

const openaiVideos: VideoApi = {
  id: "openai-videos",
  label: "OpenAI Videos",
  endpoint: "/v1/videos",
  create: (model, prompt, seconds) => ({ path: "/v1/videos", body: { model, prompt, seconds } }),
  read: (job) => ({
    id: job.id,
    status: job.status,
    progress: job.progress ?? undefined,
    error: job.error?.message ?? undefined,
  }),
  poll: (job) => `/v1/videos/${encodeURIComponent(job.id)}`,
  content: (job) => `/v1/videos/${encodeURIComponent(job.id)}/content`,
};

const generated = (op: Json) => op.response?.generateVideoResponse;

/**
 * Gemini Omni: a long-running operation, polled by its name until done,
 * its video at the URI it names (the gateway's, whoever made it).
 */
const geminiOmni: VideoApi = {
  id: "gemini-omni",
  label: "Gemini Omni",
  endpoint: "/v1beta/models/{model}:predictLongRunning",
  create: (model, prompt, seconds) => ({
    path: `/v1beta/models/${encodeURIComponent(model)}:predictLongRunning`,
    body: { instances: [{ prompt }], parameters: { durationSeconds: Number(seconds) } },
  }),
  read(op) {
    const filtered = generated(op)?.raiMediaFilteredReasons?.[0];
    const failed = op.error ?? (filtered ? { message: filtered } : undefined);
    const status = !op.done ? "in_progress" : failed ? "failed" : "completed";
    return { id: op.name, status, error: failed?.message };
  },
  poll: (job) => `/v1beta/${job.id}`,
  content(_job, op) {
    const uri: string = generated(op)?.generatedSamples?.[0]?.video?.uri ?? "";
    // At the gateway, whatever host it names: its path, through this page's.
    const url = new URL(uri, window.location.origin);
    return `${url.pathname}${url.search}`;
  },
};

export const videoApis: VideoApi[] = [openaiVideos, geminiOmni];
