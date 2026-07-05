Implement a Wayland compositor with an MCP server as the primary interface.
You may install any application inside this dev-container to test or debug the application.
The final result should be a Rust/cargo project building the executable.

## CLI interface

When starting the application using the `fork` option, it shall span a deamon by forking itself.
The initial command shall return after the startup is complete and print the necessary environment to *stdout*.

```
eval "$(desktop-mcp fork)"
```

You may also support a non-forking mode, which would not support dynamic allocation of sockets.
At least I would not have a good idea how such dynamic allocation could work, so that the environment variables are made available for the caller.

## MCP Design Idea

To facilitate the interactive nature of UIs, each MCP tool returns the new state of the UI.
The model does not have to manually request new screenshots.
The steps of each tool call roughly looks like this:

 1. The LLM calls any tool of the MCP server
 2. The MCP server triggers the requested action (e.g. typing, mouse press, mouse hold)
 3. The MCP starts tracking any changes on any relevant Window (this may be all Windows, or maybe just the Window the Model is interacting with.)
 4. The MCP server waits until the application has finished its transition.
    - Wait for the Window taking any action (open dialog, updating the UI, ...)
    - Repetatly sends ping requests. Once we made two ping-pong-cycles without the application making changes on the UI, the transition is considered complete.
    - If the UI is not converging within 3 seconds (make it configurable as tool input), continue with the current state
 5. The MCP returns
    - High-level state: i.e. list of currently open windows and their title. Mark them if they are frozen (i.e. did not respond to ping-requests for 10 seconds)
    - The state of each Window which received updates after step 2, including a screenshot and accessibility metadata
    - In the tool input, the model may specify a specific window to watch, in which case only updates to this window and newly created windows are reported

To simplify design, the Compositor sets the display resolution to a reslution matching the LLMs visual capabilities.

## MCP Tools

- A tool to see the current state of any window, a list of windows, or all windows at once.
  This tool effectivly starts at step 3 of the previously described process, except that we don't wait for an action, and we return the window states even if nothing changes.
- Wait for the next screen. This tool can be used when the LLM receives a loading indicator.
  This tool is very similar to the show status tool. We start at step 3, but this time include the initial step to wait for an action.
  This tool also requires more then two ping-pong-cycle to handle loading indicators with less frequent updates. About 1.5 seconds without updates (but working ping-pong-cycles) should be the default and enough for most UIs, but the value should be configurable.
  This tool also requires the timeout-input which is optional for all the other tools. The default of 3 seconds is probably not appropriate for this call, as we might expect timeouts mesured in minutes.
- Click, hold, release mouse button.
- Type a text
- Type a specific keyboard button
- ...

## Monitoring Capabilities for Humans

There must be some capability to let a human monitor the actions on the UI.
However, interactions are only possible via the MCP.
The UI is purely for monitoring the actions for debugging purposes.

Potential implementations:

- VNC Server
- noVNC-like webinterface

## Stack

The application is a Rust project using `smithay`, `rmcp` and `atspi`.
You may also add other commonly used libraries.

Read through the following links to get a more detailed perspective.

- https://share.google/aimode/cPwfjsFdXB3V09fdg (also available as wayland-compositor-library.md)
- https://share.google/aimode/ChCB7x1SI6cv2KrJR (also available as rust-mcp-framework.md)
- https://share.google/aimode/m1mCIug0USzADHcpZ (also available as llm-pointing-support.md)
- https://platform.claude.com/docs/en/build-with-claude/vision-coordinates

## Validation

I already configured an MCP for yourself at `http://127.0.0.1:8080/mcp`. Once you start the MCP under this URL, you should be able to use it directly.

---

You may diverge from this design, but you must report all differences to the plan in a file you create at `report.md`. Provide the reason for each divergence.

Unfortunately I did not manage to get `apt` wokring in type. If you need to install something, you try to find a workaround.

