use super::state::{MessageState, MessageType};

fn msg(id: &str, data: &str, expanded: bool, children: Vec<MessageState>) -> MessageState {
    MessageState::new(id)
        .text(data)
        .data(data)
        .expanded(expanded)
        .children(children)
}

fn tool_msg(id: &str, tool_name: &str, tool_params: &str) -> MessageState {
    MessageState::new(id)
        .text(format!("{}: {}", tool_name, tool_params))
        .message_type(MessageType::ToolCall)
        .expanded(false)
        .children(vec![])
}

fn turn_group(n: usize, brief: &str, children: Vec<MessageState>) -> MessageState {
    MessageState::new(format!("turn:{n}"))
        .brief(brief)
        .group(true)
        .expanded(true)
        .message_type(MessageType::Container)
        .indent_children(false)
        .children(children)
}

pub fn sample_tree() -> Vec<MessageState> {
    vec![
        turn_group(
            0,
            "Can you read the config file and summarise what it does?",
            vec![
                // Turn 1: User message
                msg(
                    "u1",
                    "User: Can you read the config file and summarise what it does? I want to understand the overall structure, what services it configures, and whether there are any obvious misconfigurations or deprecated settings I should know about before we deploy to staging.",
                    false,
                    vec![],
                ),
                // Turn 2: Agent turn (expanded, contains sub-messages)
                msg(
                    "a1",
                    "Agent Turn 1",
                    true,
                    vec![
                        MessageState::new("a1-thinking")
                            .text("The user wants me to read config.toml and summarise it. I should look at the structure, identify services, and flag any misconfigurations or deprecated settings. Let me fetch the file first.")
                            .message_type(MessageType::Thinking),
                        msg(
                            "a1-text1",
                            "Sure, let me read the config file for you. I'll take a careful look at the structure and flag anything that looks suspicious or non-standard. One moment while I fetch the contents.",
                            false,
                            vec![],
                        ),
                        tool_msg("a1-tool1", "Read", "\"config.toml\""),
                        msg(
                            "a1-text2",
                            "The config file sets up four main subsystems: database connectivity, logging, feature flags, and rate limiting.\n\nDatabase: points to localhost:5432 using a connection pool of 10. The pool timeout is 30 seconds, which is on the high side — you may want to lower this to 5–10s to avoid holding idle connections under load.\n\nLogging: set to INFO globally, with DEBUG overridden for the `auth` and `payments` modules. Structured JSON output is enabled, which is good for log aggregation.\n\nFeature flags: three flags are active — dark_mode, new_dashboard, and beta_search. The beta_search flag has no rollout percentage configured, meaning it's on for 100% of users; that may be intentional but worth confirming.\n\nRate limiting: 1000 requests/minute per IP, with a burst allowance of 50. No per-user rate limits are configured, which could be a gap if you have authenticated endpoints you want to protect independently.",
                            false,
                            vec![],
                        ),
                    ],
                ),
            ],
        ),
        turn_group(
            1,
            "Can you also check if there are any security issues?",
            vec![
                msg(
                    "u2",
                    "User: Can you also check if there are any security issues? I'm particularly worried about injection vulnerabilities and anything related to authentication — we had a near-miss last quarter and I want to make sure we've tightened things up before this release.",
                    false,
                    vec![],
                ),
                msg(
                    "a2",
                    "Agent Turn 2",
                    true,
                    vec![
                        msg(
                            "a2-text1",
                            "Understood — given the history you mentioned, I'll be thorough. I'll run a dependency audit first to check for known CVEs in our third-party packages, then spawn a dedicated security-reviewer sub-agent to do a deeper static analysis pass across the auth and data-access layers. This should give us a comprehensive picture before staging.",
                            false,
                            vec![],
                        ),
                        tool_msg("a2-tool1", "Bash", "\"cargo audit\""),
                        msg(
                            "a2-subagent",
                            "Sub-Agent: security-reviewer",
                            true,
                            vec![
                                msg(
                                    "sa1-prompt",
                                    "Sub-Agent Prompt: Review the codebase for OWASP Top-10 vulnerabilities with a focus on: (1) injection flaws in all database query sites, (2) authentication and session management weaknesses, (3) insecure direct object references in API handlers, and (4) any use of unsafe Rust that could lead to memory corruption. Report each finding with severity, affected file and line range, and a recommended fix.",
                                    false,
                                    vec![],
                                ),
                                tool_msg("sa1-tool1", "Read", "\"src/auth.rs\""),
                                tool_msg("sa1-tool2", "Read", "\"src/db/queries.rs\""),
                                tool_msg("sa1-tool3", "Grep", "\"unwrap()\", \"src/\""),
                                tool_msg("sa1-tool4", "Grep", "\"format!\", \"src/db/\""),
                                msg(
                                    "sa1-response",
                                    "Sub-Agent Response: Found 5 issues across 3 severity levels.\n\nCRITICAL:\n1. SQL injection in src/db/queries.rs:47\n2. Session token not rotated on privilege escalation in src/auth.rs:112.\n\nHIGH:\n3. IDOR in src/api/documents.rs:88\n\nMEDIUM:\n4. 47 unchecked unwrap() calls\n5. RUSTSEC-2024-0019 in hyper 0.14.x",
                                    false,
                                    vec![],
                                ),
                            ],
                        ),
                        msg(
                            "a2-text2",
                            "The sub-agent surfaced five findings. The two criticals need to be addressed before staging.",
                            false,
                            vec![],
                        ),
                    ],
                ),
            ],
        ),
        turn_group(
            2,
            "Fix the SQL injection issue first.",
            vec![
                msg(
                    "u3",
                    "User: Fix the SQL injection issue first. Then fix the session token rotation. Let's tackle them one at a time so the diff stays reviewable.",
                    false,
                    vec![],
                ),
                msg(
                    "a3",
                    "Agent Turn 3",
                    true,
                    vec![
                        msg(
                            "a3-text1",
                            "Good call — keeping the diffs small makes review much easier.",
                            false,
                            vec![],
                        ),
                        tool_msg("a3-tool1", "Read", "\"src/db/queries.rs\""),
                        tool_msg("a3-tool2", "Edit", "\"src/db/queries.rs\", ..."),
                        msg(
                            "a3-text2",
                            "SQL injection fix applied. The query now uses bound parameters.",
                            false,
                            vec![],
                        ),
                    ],
                ),
            ],
        ),
        turn_group(
            3,
            "Run the test suite to confirm nothing is broken.",
            vec![
                msg(
                    "u4",
                    "User: Run the test suite to confirm nothing is broken.",
                    false,
                    vec![],
                ),
                // Pre-collapsed turn
                msg(
                    "a4",
                    "Agent Turn 4",
                    false,
                    vec![
                        tool_msg("a4-tool1", "Bash", "\"cargo test 2>&1\""),
                        tool_msg("a4-tool2", "Bash", "\"cargo test auth -- --nocapture 2>&1\""),
                        msg(
                            "a4-text1",
                            "Full test suite: 142 tests passed, 0 failed. Auth-specific tests: 23 tests all passed.",
                            false,
                            vec![],
                        ),
                    ],
                ),
            ],
        ),
        turn_group(
            4,
            "Great. Now fix the IDOR issue.",
            vec![
                msg(
                    "u5",
                    "User: Great. Now fix the IDOR issue in the documents endpoint. Once that's done, give me a summary of all the changes we made today so I can write the PR description.",
                    false,
                    vec![],
                ),
                msg(
                    "a5",
                    "Agent Turn 5",
                    true,
                    vec![
                        msg(
                            "a5-text1",
                            "On it. The IDOR fix in src/api/documents.rs is a single ownership check.",
                            false,
                            vec![],
                        ),
                        tool_msg("a5-tool1", "Read", "\"src/api/documents.rs\""),
                        tool_msg("a5-tool2", "Edit", "\"src/api/documents.rs\", ..."),
                        msg(
                            "a5-text2",
                            "IDOR fix applied. Here's a summary of all changes:\n\n**1. SQL injection fix** (`src/db/queries.rs:47`)\n**2. Session token rotation** (`src/auth.rs:112`)\n**3. IDOR in document fetch** (`src/api/documents.rs:88`)\n\nAll 142 tests pass.",
                            false,
                            vec![],
                        ),
                    ],
                ),
            ],
        ),
    ]
}
