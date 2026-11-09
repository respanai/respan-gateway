// The logos of common vendors, bundled: each by the gateway's vendor id, and
// by its API's domain for a provider configured without one. Mono logos take
// the text's color; the others keep their own. (LobeHub's icons, MIT.)

import anthropic from "@lobehub/icons-static-svg/icons/anthropic.svg?url";
import azure from "@lobehub/icons-static-svg/icons/azure-color.svg?url";
import cerebras from "@lobehub/icons-static-svg/icons/cerebras-color.svg?url";
import cloudflare from "@lobehub/icons-static-svg/icons/cloudflare-color.svg?url";
import deepinfra from "@lobehub/icons-static-svg/icons/deepinfra-color.svg?url";
import deepseek from "@lobehub/icons-static-svg/icons/deepseek-color.svg?url";
import fireworks from "@lobehub/icons-static-svg/icons/fireworks-color.svg?url";
import gemini from "@lobehub/icons-static-svg/icons/gemini-color.svg?url";
import groq from "@lobehub/icons-static-svg/icons/groq.svg?url";
import mistral from "@lobehub/icons-static-svg/icons/mistral-color.svg?url";
import ollama from "@lobehub/icons-static-svg/icons/ollama.svg?url";
import openai from "@lobehub/icons-static-svg/icons/openai.svg?url";
import openrouter from "@lobehub/icons-static-svg/icons/openrouter.svg?url";
import together from "@lobehub/icons-static-svg/icons/together-color.svg?url";
import vllm from "@lobehub/icons-static-svg/icons/vllm-color.svg?url";
import xai from "@lobehub/icons-static-svg/icons/xai.svg?url";

export interface VendorIcon {
  src: string;
  /** Drawn in the text's color, as a mask. */
  mono: boolean;
}

const icons: Record<string, VendorIcon> = {
  anthropic: { src: anthropic, mono: true },
  azure: { src: azure, mono: false },
  cerebras: { src: cerebras, mono: false },
  cloudflare: { src: cloudflare, mono: false },
  deepinfra: { src: deepinfra, mono: false },
  deepseek: { src: deepseek, mono: false },
  fireworks: { src: fireworks, mono: false },
  gemini: { src: gemini, mono: false },
  groq: { src: groq, mono: true },
  mistral: { src: mistral, mono: false },
  ollama: { src: ollama, mono: true },
  openai: { src: openai, mono: true },
  openrouter: { src: openrouter, mono: true },
  together: { src: together, mono: false },
  vertex: { src: gemini, mono: false },
  "vertex-express": { src: gemini, mono: false },
  vllm: { src: vllm, mono: false },
  xai: { src: xai, mono: true },
};

/** Each vendor's API domain, as its base URL ends. */
const domains: [string, string][] = [
  ["anthropic.com", "anthropic"],
  ["azure.com", "azure"],
  ["cerebras.ai", "cerebras"],
  ["cloudflare.com", "cloudflare"],
  ["deepinfra.com", "deepinfra"],
  ["deepseek.com", "deepseek"],
  ["fireworks.ai", "fireworks"],
  ["googleapis.com", "gemini"],
  ["groq.com", "groq"],
  ["mistral.ai", "mistral"],
  ["openai.com", "openai"],
  ["openrouter.ai", "openrouter"],
  ["together.ai", "together"],
  ["together.xyz", "together"],
  ["x.ai", "xai"],
];

/** The logo of `vendor`, else of the vendor whose API is at `domain`; none for others. */
export function vendorIcon(vendor: string | null | undefined, domain: string | undefined): VendorIcon | undefined {
  const byDomain = domain && domains.find(([d]) => domain === d || domain.endsWith(`.${d}`))?.[1];
  return icons[vendor ?? ""] ?? (byDomain ? icons[byDomain] : undefined);
}
