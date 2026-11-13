#![warn(missing_docs)]
//! The tool vendors: HTTP APIs offered as tools, in two tiers, each a
//! crate: green (`respan-gateway-tool-vendors-green`), maintained and
//! verified end to end against their live APIs, and yellow
//! (`respan-gateway-tool-vendors-yellow`), maintained but not verified end
//! to end. [`ALL`] lists them; [`find`] looks one up by its id.
//!
//! - [`apify`]: Apify's Store of scraping Actors: finding one, running it and
//!   reading its items, and Google search and page reads through its RAG Web
//!   Browser (`search_actors`, `run_actor`, `get_run_items`, `browse_web`), its
//!   key from `APIFY_TOKEN` or `APIFY_API_TOKEN`.
//! - [`browserbase`]: Browserbase's web search, page fetches, browser agent
//!   and live views (`search`, `fetch`, `run_agent`, `get_agent_run`,
//!   `stop_agent_run`, `live_view`), its key from `BROWSERBASE_API_KEY`.
//! - [`cloudinary`]: Cloudinary's image and video uploads, search, asset
//!   details, transformation URLs and deletion (`upload`, `search`,
//!   `get_asset`, `delivery_url`, `delete_asset`), its key from
//!   `CLOUDINARY_URL`.
//! - [`context7`]: Context7's up-to-date library documentation
//!   (`resolve_library_id`, `query_docs`, `search_docs`), its key from
//!   `CONTEXT7_API_KEY`.
//! - [`daytona`]: Daytona's code sandboxes: create, list and delete them, and
//!   run commands and code and write files in them (`create_sandbox`,
//!   `list_sandboxes`, `delete_sandbox`, `run_command`, `run_code`,
//!   `write_file`), its key from `DAYTONA_API_KEY`.
//! - [`deedpage`]: deed.page's static HTML hosting: publishing a page to a
//!   public URL, listing sites and deleting one (`publish_site`,
//!   `list_sites`, `delete_site`), its key from `DEEDPAGE_TOKEN`.
//! - [`e2b`]: E2B's cloud sandboxes, running code (`run_code`,
//!   `list_sandboxes`, `kill_sandbox`), its key from `E2B_API_KEY`.
//! - [`elevenlabs`]: ElevenLabs's speech, voices, transcripts and sound
//!   effects (`text_to_speech`, `search_voices`, `speech_to_text`,
//!   `sound_effect`), its key from `ELEVENLABS_API_KEY`.
//! - [`exa`]: Exa's web search and page contents (`search`, `get_contents`),
//!   its key from `EXA_API_KEY`.
//! - [`firecrawl`]: Firecrawl's page scraping, site crawls, site maps and
//!   web search (`scrape`, `crawl`, `get_crawl`, `map_site`, `search`), its
//!   key from `FIRECRAWL_API_KEY`.
//! - [`linkup`]: Linkup's web search, sourced answers and page fetches
//!   (`search`, `answer`, `fetch`), its key from `LINKUP_API_KEY`.
//! - [`reducto`]: Reducto's document parsing, field extraction and
//!   section splitting (`parse_document`, `extract_fields`,
//!   `split_document`), its key from `REDUCTO_API_KEY`.
//! - [`tavily`]: Tavily's web search, page extraction, site crawls and site
//!   maps (`search`, `extract`, `crawl`, `map_site`), its key from
//!   `TAVILY_API_KEY`.
//! - [`tinyfish`]: TinyFish's web search, page fetches and browser agent
//!   (`search`, `fetch`, `run_agent`), its key from `TINYFISH_API_KEY`.
//! - [`zep`]: Zep's agent memory: knowledge-graph search, context blocks,
//!   and adding messages and data (`search`, `get_graph_context`,
//!   `get_thread_context`, `add_messages`, `add_data`), its key from
//!   `ZEP_API_KEY`.

pub use respan_gateway_tool_vendor_kit::*;

pub use respan_gateway_tool_vendors_green::{
    apify, browserbase, cloudinary, context7, daytona, linkup, reducto, tavily, tinyfish, zep,
};
pub use respan_gateway_tool_vendors_yellow::{deedpage, e2b, elevenlabs, exa, firecrawl};

/// The green tool vendors: maintained, and verified end to end.
pub const GREEN: &[&ToolVendor] = respan_gateway_tool_vendors_green::ALL;

/// The yellow tool vendors: maintained, not verified end to end.
pub const YELLOW: &[&ToolVendor] = respan_gateway_tool_vendors_yellow::ALL;

/// Every tool vendor.
pub const ALL: &[&ToolVendor] = &[
    &apify::VENDOR,
    &browserbase::VENDOR,
    &cloudinary::VENDOR,
    &context7::VENDOR,
    &daytona::VENDOR,
    &deedpage::VENDOR,
    &e2b::VENDOR,
    &elevenlabs::VENDOR,
    &exa::VENDOR,
    &firecrawl::VENDOR,
    &linkup::VENDOR,
    &reducto::VENDOR,
    &tavily::VENDOR,
    &tinyfish::VENDOR,
    &zep::VENDOR,
];

/// The tool vendor whose id is `id`.
pub fn find(id: &str) -> Option<&'static ToolVendor> {
    ALL.iter().copied().find(|v| v.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::collections::HashSet;

    #[test]
    fn every_vendor_is_in_one_tier() {
        let tiers: Vec<_> = GREEN.iter().chain(YELLOW).map(|v| v.id).collect();
        let all: Vec<_> = ALL.iter().map(|v| v.id).collect();
        assert_eq!(tiers.len(), all.len(), "{tiers:?} {all:?}");
        for id in &all {
            assert!(tiers.contains(id), "{id} is in no tier");
        }
    }

    #[test]
    fn vendors_are_well_formed() {
        let ids: HashSet<_> = ALL.iter().map(|v| v.id).collect();
        assert_eq!(ids.len(), ALL.len(), "vendor ids are unique");
        for vendor in ALL {
            assert_eq!(find(vendor.id).map(|v| v.id), Some(vendor.id));
            let tools = vendor.tools.list();
            assert!(!tools.is_empty(), "{} has tools", vendor.id);
            let names: HashSet<_> = tools.iter().map(|t| t.name).collect();
            assert_eq!(
                names.len(),
                tools.len(),
                "{}'s tool names are unique",
                vendor.id
            );
            for tool in tools {
                let label = format!("{}.{}", vendor.id, tool.name);
                assert!(
                    tool.name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                    "{label} is snake_case"
                );
                assert!(tool.name.len() <= 32, "{label} is short");
                assert!(!tool.description.is_empty(), "{label} is described");
                let schema: Value = serde_json::from_str(tool.input_schema)
                    .unwrap_or_else(|e| panic!("{label}'s schema parses: {e}"));
                assert_eq!(schema["type"], "object", "{label}'s schema is an object");
                let properties = schema["properties"].as_object().expect("properties");
                for required in schema["required"].as_array().into_iter().flatten() {
                    let required = required.as_str().expect("a name");
                    assert!(
                        properties.contains_key(required),
                        "{label} requires {required}, a property"
                    );
                }
            }
        }
        assert!(find("nope").is_none());
    }
}
