## cli / empty-llm-response

Simulates the LLM returning a successful turn with zero content blocks
(no text, no tool_use). The runner currently treats this as a clean
"answered with nothing": inbound -> completed, no chat outbound row,
and the usage_report system row still emitted. This fixture pins that
behaviour so a regression that crashes on empty content would surface.
