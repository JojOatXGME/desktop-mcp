Is there a good MCP framework for Rust?14:25Yes, the industry standard and most widely recommended framework is the official [Rust SDK for the Model Context Protocol](https://github.com/modelcontextprotocol/rust-sdk). It is maintained with first-party support, integrates cleanly with async runtimes like [Tokio](https://docs.rs/tokio/latest/tokio/), and provides a macro-driven development experience. \[[1](https://medium.com/@ed-burton/six-months-of-running-mcp-servers-in-rust-what-id-do-differently-1ee52f68225a), [2](https://rustify.rs/articles/rust-for-mcp-model-context-protocol-servers-2026), [3](https://github.com/modelcontextprotocol/rust-sdk)\]**The Official Rust SDK (rmcp)**The SDK revolves around the rmcp crate, which provides a macro-heavy, ergonomic API so you don’t have to manually write JSON-RPC serialization code. \[[1](https://medium.com/@ed-burton/six-months-of-running-mcp-servers-in-rust-what-id-do-differently-1ee52f68225a), [2](https://github.com/modelcontextprotocol/rust-sdk)\]**Key Features:**

*   **#\[tool(...)\] macros:** Expose Rust methods as AI tools by simply annotating your struct methods. \[[1](https://medium.com/@ed-burton/six-months-of-running-mcp-servers-in-rust-what-id-do-differently-1ee52f68225a)\]
    
*   **Zero-effort JSON Schemas:** By deriving JsonSchema (via [schemars](https://docs.rs/schemars/latest/schemars/)) on your parameter structs, the framework automatically builds and validates the parameter definitions for the AI. \[[1](https://medium.com/@ed-burton/six-months-of-running-mcp-servers-in-rust-what-id-do-differently-1ee52f68225a), [2](https://github.com/modelcontextprotocol/rust-sdk)\]
    
*   **Full Spec Support:** Includes first-party implementations for Tools, Prompts, Resources, Completions, and Logging. \[[1](https://github.com/modelcontextprotocol/rust-sdk)\]
    
*   **Multiple Transports:** Supports both stdio (standard input/output) for local IDE integrations (like [Cursor](https://www.cursor.com/) or [Claude Desktop](https://claude.ai/download)) and SSE (Server-Sent Events) for networked environments. \[[1](https://www.reddit.com/r/rust/comments/1jglry7/introducing_rmcp_maybe_the_best_rust_mcp_sdk/), [2](https://github.com/modelcontextprotocol/rust-sdk)\]
    

**How to use it**To get started, you will typically depend on the rmcp and rmcp-macros crates. \[[1](https://github.com/modelcontextprotocol/rust-sdk)\]Here is a quick look at how clean tool creation looks in practice:rust

Plain textANTLR4BashCC#CSSCoffeeScriptCMakeDartDjangoDockerEJSErlangGitGoGraphQLGroovyHTMLJavaJavaScriptJSONJSXKotlinLaTeXLessLuaMakefileMarkdownMATLABMarkupObjective-CPerlPHPPowerShell.propertiesProtocol BuffersPythonRRubySass (Sass)Sass (Scss)SchemeSQLShellSwiftSVGTSXTypeScriptWebAssemblyYAMLXML`   use rmcp::macros::{tool, tool_box};  use schemars::JsonSchema;  use serde::Deserialize;  #[derive(Deserialize, JsonSchema)]  struct CalculatorRequest {      a: f64,      b: f64,  }  #[tool_box]  struct MyTools;  #[tool_box]  impl MyTools {      #[tool(description = "Adds two numbers together")]      fn add(&self, args: CalculatorRequest) -> anyhow::Result {          let result = args.a + args.b;          Ok(format!("The result is {}", result))      }  }   `

Verwende Code mit Vorsicht.**Alternative Options**While rmcp is the consolidated standard, there are other community-driven projects if you have specific architectural needs:

*   [Turul MCP Framework](https://github.com/aussierobots/turul-mcp-framework): A modern, enterprise-focused Rust framework for building both MCP servers and clients.
    
*   **rust-mcp-server**: A popular crate available on [crates.io](https://crates.io/crates/rust-mcp-server) built specifically to bridge local tools and project context with AI coding assistants. \[[1](https://github.com/aussierobots/turul-mcp-framework), [2](https://crates.io/crates/rust-mcp-server)\]
    

For tips on how to build and containerize an MCP server in Rust:30:34[Build an MCP Server using Rust and Docker - AI and the ...Francesco CiullaYouTube · 21.04.2026](https://www.youtube.com/watch?v=gGRJpwyCAio&vl=de)If you'd like, tell me:

*   What **specific tools or integrations** (e.g., databases, local files, internal APIs) are you building?
    
*   Do you prefer **Stdio** (local execution) or **SSE** (networked/remote)?
    

I can help you scaffold the starter code or select the right transport method.KI-Antworten können Fehler enthalten. [Weitere Informationen](https://support.google.com/websearch?p=aimode)6 Websites

*   Six Months of Running MCP Servers in Rust. What I'd Do ...What actually works is simple and boring. The rmcp crate, which now has over 4.7 million downloads on crates.io as the official Ru...Medium
    
*   Introducing rmcp, maybe the best Rust MCP SDK implementation by ...\* All the features are implemented. Including ping, cancellation, progress... \* Strictly followed the MCP specification. Check the...Reddit
    
*   The official Rust SDK for the Model Context Protocol - GitHubUh oh! \* Notifications You must be signed in to change notification settings. \* Fork 550. \* Star 3.6k. \* Issues 30. \* Pull request...GitHub
    

Alle anzeigen

You said: Does this library support returning images to the LLM from a tool?
----------------------------------------------------------------------------

Does this library support returning images to the LLM from a tool?14:34**Yes, the official Rust SDK (rmcp) fully supports returning images from a tool to the LLM****.** \[[1](https://docs.rs/mcp-core), [2](https://gitlab.com/lx-industries/rmcp-openapi)\]Per the underlying Model Context Protocol specification, images are sent back as multi-modal content types using base64 encoding along with a matching MIME type (e.g., image/png). \[[1](https://github.com/modelcontextprotocol/python-sdk/issues/2557), [2](https://gitlab.com/lx-industries/rmcp-openapi)\]**How It Works in Rust**Instead of returning a simple String from your #\[tool\] function, you return a structured CallToolResult (or ImageContent) payload provided by the SDK. \[[1](https://forum.langchain.com/t/why-can-the-model-see-the-structured-content-returned-by-the-mcp-tool/3076), [2](https://docs.rs/mcp-core)\]The mcp-core and rmcp crates provide a dedicated macro helper—tool\_image\_content!—specifically for constructing this structure: \[[1](https://docs.rs/mcp-core)\]rust

Plain textANTLR4BashCC#CSSCoffeeScriptCMakeDartDjangoDockerEJSErlangGitGoGraphQLGroovyHTMLJavaJavaScriptJSONJSXKotlinLaTeXLessLuaMakefileMarkdownMATLABMarkupObjective-CPerlPHPPowerShell.propertiesProtocol BuffersPythonRRubySass (Sass)Sass (Scss)SchemeSQLShellSwiftSVGTSXTypeScriptWebAssemblyYAMLXML`   use rmcp::macros::{tool, tool_box, tool_image_content};  use anyhow::Result;  // Assuming you've pulled in CallToolResult and ImageContent from the SDK  use rmcp::protocol::types::{CallToolResult, ImageContent};   #[tool_box]  struct MediaTools;  #[tool_box]  impl MediaTools {      #[tool(description = "Generates a chart or image and returns it to the AI")]      fn generate_chart(&self, args: ChartRequest) -> Result {          // 1. Generate your raw image bytes (e.g., via a charting library)          let raw_bytes: Vec = render_my_chart(&args)?;          // 2. Encode the raw bytes into a base64 String          let base64_data = base64::encode(raw_bytes);          // 3. Construct the Image Content wrapper using the SDK macro          // This tells the LLM host (like Claude or Cursor) to process it as a visual block          let image_block = tool_image_content!(              data = base64_data,              mime_type = "image/png"          );          // 4. Return the multimodal payload          Ok(CallToolResult {              content: vec![image_block],              is_error: false,          })      }  }   `

Verwende Code mit Vorsicht.**⚠️ Crucial Client Limitations to Keep in Mind**While the Rust SDK perfectly packs and serializes the image data, **the host UI / Client app you are using dictates whether the model actually sees it**. \[[1](https://github.com/modelcontextprotocol/python-sdk/issues/2557), [2](https://gitlab.com/lx-industries/rmcp-openapi)\]

*   **Claude Desktop:** Fully supports image blocks returned by tools. It displays the image directly inside the chat window so the multimodal model can analyze the visual elements. \[[1](https://medium.com/@davidlfliang/guide-introduction-to-mcp-part-4-creating-visuals-with-mcp-server-f7864d30f627), [2](https://github.com/modelcontextprotocol/python-sdk/issues/2557)\]
    
*   **Cursor / Windsurf:** Support varies depending on the active model configuration and whether the client IDE converts the tool response JSON directly into the context window correctly. \[[1](https://docs.rustfs.com/developer/mcp.html), [2](https://github.com/CherryHQ/cherry-studio/issues/10991)\]
    
*   **Alternative "Resource" Method:** If you are passing exceptionally large image payloads (which can occasionally hit string buffer limits on certain local Stdio host wrappers), the recommended protocol alternative is to expose the image as an [MCP Resource](https://modelcontextprotocol.io/docs/concepts/resources) via read\_resource() rather than forcing a massive base64 blob inline through a tool call. \[[1](https://github.com/modelcontextprotocol/python-sdk/issues/2557)\]
    

Would you like assistance setting up an **MCP Resource route** to handle images instead, or do you need help **integrating a specific image generation library** (like plotters or image) into your tool chain?