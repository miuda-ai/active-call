# Context Repair & Rolling Summary

`active-call` provides advanced context management features to handle complex conversational scenarios.

## Context Repair

This feature addresses the "fragmentation" issue caused by pauses in speech.
For example:
1. User: "My car broke down..."
2. (Pause)
3. Bot: "Where are you?"
4. User: "...at the station."

Without context repair, the bot would likely repeat the question "Where are you?".
With context repair, the system detects that the user's second statement is a continuation of the first, removes the bot's interruption, and merges the user's messages.
The LLM sees: User: "My car broke down... at the station."

### Configuration

Enable this feature by adding `"context_repair"` to the `features` list in your playbook `llm` config.

```markdown
---
llm:
  features: 
    - "context_repair"
  repair_window_ms: 3000 # Window to detect continuation (ms), default: 3000
---
```

## Rolling Summary

For long conversations, this feature automatically summarizes the history to prevent token overflow while maintaining context.

### Configuration

Enable this feature by adding `"rolling_summary"` to the `features` list.

```markdown
---
llm:
  features: 
    - "rolling_summary"
  summary_limit: 20 # Number of messages before triggering summary, default: 20
---
```
