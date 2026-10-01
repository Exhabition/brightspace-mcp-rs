# Brightspace MCP (Rust)

Read-only MCP server for D2L Brightspace. Backend is Rust; transport is MCP stdio.

Authentication uses one path only: a visible Chromium browser opens Brightspace, Microsoft SSO handles sign-in, and the user approves MFA in Microsoft Authenticator. This server does not accept API tokens, Brightspace OAuth, pasted cookies, usernames, or passwords.

## Requirements

- Node.js 18 or newer (for the `npx` launcher)
- Chrome or Chromium installed
- A Brightspace site that authenticates through Microsoft SSO
- Rust 1.85 or newer only for local builds

## Run

For local development:

```sh
cargo run -- --base-url https://learn.example.edu
```

For regular use, install Node.js 18+ and use the public npm package. On first start, the launcher downloads the matching binary for your operating system and CPU from the public GitHub release, then reuses it afterward. Chrome or Chromium remains required for sign-in.

```sh
npx -y @exhabition/brightspace-mcp --base-url https://learn.example.edu
```

Or set `BRIGHTSPACE_BASE_URL`. The browser stays open for the MCP server lifetime. Sync fetches all enrolled courses and writes local snapshots; course tools and resources read those snapshots without contacting Brightspace. After an MCP restart, cached reads still work. Run `sync_courses` when you want fresh data; Microsoft may ask for password and MFA again if the school does not persist browser sessions.

Browser profile defaults to `~/.brightspace-mcp-rs/browser-profile`. Override with `--browser-profile` or `BRIGHTSPACE_BROWSER_PROFILE`. The profile contains Microsoft and Brightspace browser session data, so keep it private. On Unix, the server sets its directory permissions to `0700`. The process keeps the Brightspace session cookie in memory; it does not write that cookie or a password to a separate file. Set `BRIGHTSPACE_CHROME_PATH` to the Chromium executable if auto-detection fails.

Course snapshots default to `~/.brightspace-mcp-rs/synced-courses`. Override with `--sync-dir` or `BRIGHTSPACE_SYNC_DIR`. Snapshot directories use mode `0700` and files use `0600` on Unix. Snapshots include grades, submissions, course content, classlist emails, and downloadable files linked from synced course content, topics, modules, syllabus, and announcements. `get_course_file` reads these locally by their `/content/enforced/{course_id}-…` path. Sync can only discover files Brightspace links in returned content; unlinked files are not enumerable. Treat snapshots as sensitive student records. `clear_cache` deletes snapshots; it does not delete the browser profile.

The school's Microsoft Entra session policy controls how often SSO asks for MFA. If Microsoft shows a “Stay signed in?” prompt, choosing Yes may retain the browser session; tenant policy can hide or override that choice. A retained browser profile can allow silent SSO, but cannot bypass a policy that requires reauthentication. Brightspace session expiry can also trigger a fresh sign-in. Upstream Brightspace MCP documentation reports session cookies commonly expire in about an hour.

## MCP surface

Workflow tools: `sync_courses` refreshes all course snapshots; `list_synced_courses` and `read_synced_course` read them locally. Existing read tools and resource templates use local snapshots too. `check_auth` reports local sync status. Only `sync_courses` contacts authenticated Brightspace APIs. A missing or failed dataset returns a cache miss or sync error; resync to retry.

Course read tools: `list_my_courses`, `get_my_grades`, `get_assignments`, `get_upcoming_due_dates`, `get_feedback`, `get_assignment_rubric`, `get_my_submissions`, `get_roster`, `get_classlist_emails`, `get_syllabus`, `get_course_content`, `get_announcements`, `get_announcement`, `get_discussions`, `get_calendar_events`, `get_assignment_files`, `get_topic_file`, `get_module`, `list_quizzes`, `get_quiz_attempts`, `list_notifications`, `search_course`, and `get_my_groups`. Local tools: `clear_cache`, `get_diagnostics`, and `get_audit_log`.

Resource templates:

- `brightspace://{courseId}/syllabus`
- `brightspace://{courseId}/content/topics/{topicId}`
- `brightspace://{courseId}/assignments/{assignmentId}/files`
- `brightspace://{courseId}/announcements/{announcementId}`

## Client configuration

Example stdio config using npx:

```json
{
  "mcpServers": {
    "brightspace": {
      "command": "npx",
      "args": ["-y", "@exhabition/brightspace-mcp", "--base-url", "https://learn.example.edu"]
    }
  }
}
```

The package is published to the public npm registry. First use needs network access to npm and GitHub Releases. Do not add Brightspace credentials to the config; sign-in happens in the visible browser.

## Reference implementation

Tool names, resource URI patterns, and Brightspace API behavior were adapted from [JhostinAleck/brightspace-mcp](https://github.com/JhostinAleck/brightspace-mcp), licensed under MIT.
