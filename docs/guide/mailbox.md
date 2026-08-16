# The mailbox: agents in a tree talking to each other

A tree of agents could already do a great deal. It nests to any depth, shares one
token ledger and one containment boundary, queues past its concurrency cap,
gives a child its own git worktree, spawns a child by role from a roster, and —
since 0.50.0 — lets a parent detach a child and read its report at a later step.

Every one of those is a **vertical** edge. A parent starts a child, a child
reports to its parent, and the whole shape is a tree because that is the only
relationship it could express. There was no horizontal edge at all: two children
investigating two subsystems could not tell each other what they found. The only
channel between them was a file one wrote and the other happened to read, which
is unaddressed, unordered, invisible to the trace, and indistinguishable from
ordinary workspace churn.

The parent's edge was also one-directional. A parent that detaches three
children receives three reports whenever they arrive; it could not say *"I need
the scout's answer before I can brief the author"* and block on that one.

0.60.0 adds the missing edge. Every agent in a tree has an address, an agent
sends a message to another agent by that address, and a read may state a bounded
wait.

## An agent has a name, and the name is an address

Before this release nothing in the crate could name **one** agent.

`AgentDef::name` looks like it should, and does not. It is a **role**: register a
`searcher` in the roster and every child spawned from it is a `searcher`. Two
children of one definition spawned in the same step is the ordinary shape of a
fan-out — it is why `AgentDef::worktree` puts a digest of the goal in the
worktree path. "Send this to the searcher" names a role that three agents are
currently playing.

So `spawn_agent` takes a new optional argument, `as`:

```json
{
  "goal": "find every call site of `resolve_path`",
  "as": "scout",
  "agent": "searcher",
  "verify_file": "found.md",
  "verify_contains": "##"
}
```

`agent` is the role — what kind of agent this is, what model serves it, how much
narrower than its parent it runs. `as` is **this one agent's** address.

The rules are short:

- An address is unique **within one tree**. A spawn asking for one already held
  is refused, and the refusal names it. Nothing is allocated: no run row, no
  agent against the containment cap, no place in the queue.
- Letters, digits, `-` and `_`, at most 64 characters. An address is retyped by
  another agent out of a goal string, and a name carrying a space or a quote is
  one that will be retyped wrong.
- `root` is reserved. It is the address of the agent at the top of the tree —
  the one agent every child can be sure exists.
- Omit `as` and one is derived: `searcher#7`, the role and the child's run id.
  It is unique because run ids are, and it cannot collide with an assigned name
  because `#` is the one character an assigned name may not contain. Every spawn
  written before 0.60.0 therefore gets an addressable child rather than merely
  continuing to work.

A parent reads the address back in the observation for any child it stopped
waiting for:

```text
[child scout (run 7) "find every call site of `resolve_path`" detached] it is
running now; its report reaches you at a later step, and you can reach it at `scout`
```

**How a child learns a sibling's address: the parent tells it.** The parent
assigned both names, so it knows both. Put them in the goal — *"the scout is
called `scout`; wait for its finding before you edit"* — and the child has what
it needs. There is deliberately no directory tool: an answer to "who is in this
tree right now" would be stale between the call and the send, and a refused send
already lists what is reachable.

## Two tools, offered only inside a tree

Neither tool exists in a flat run. An agent with nobody to talk to is not offered
a way to talk.

### `send_message`

```json
{ "to": "author", "body": "it is at src/auth.rs:210 — the `resolve` arm" }
```

One sender, one named recipient, a body of plain text. An address that names no
agent in this tree is refused **with the names that do**, so a model that
mistyped one recovers on its next step instead of guessing:

```text
[send_message error] no agent in this tree is addressed `authro`.
Reachable from here: author, root, scout
```

### `read_messages`

```json
{ "from": "scout", "wait_secs": 60 }
```

Returns everything addressed to this agent and not yet delivered, oldest first,
and marks it delivered in the same transaction. Both arguments are optional:
`from` narrows to one sender and leaves the rest waiting, and `wait_secs`
defaults to zero — a drain, never a block, for a caller that says nothing.

```text
[messages] 2 waiting
[scout @step 3] it is at src/auth.rs:210 — the `resolve` arm
[critic @step 4] the same shape exists in src/session.rs:88
```

## Waiting, and why it is always bounded

`wait_secs` blocks until something arrives or the clock runs out. There is no way
to spell "wait forever", and that absence is the design.

**An agent that blocks holds its concurrency slot.** The sibling that would
answer it may be the one queued behind that very slot. An unbounded wait turns a
tree that would have carried on into a tree that stops, so every wait has a
ceiling: `max_wait_secs` in `[run]` or `TaskContract::with_max_wait_secs`, and
30 seconds when neither is set. An agent asking for more is given the cap and
**told**:

```text
[wait narrowed] this run allows a wait of at most 5s, so that is what was waited
[messages] 1 waiting
```

The notice matters as much as the cap. An agent that believes it waited a minute
and waited five seconds draws the wrong conclusion from an empty mailbox.

`max_wait_secs` is a **narrowing** key: a project-scoped `io.toml` may lower it
and cannot raise it, the same rule `max_read_chars` follows and for the same
reason — it is a number, so there is no single widening *value* to refuse, and
the lower of the two wins.

Two things keep a wait from being wasted:

**A finished agent posts to its parent.** When an agent terminates it sends its
parent one short line — `[finished] Success { steps: 4 }` — and not its report.
That is what makes "wait for a named child" and "wait for a message" one
mechanism: a parent blocked on `from: "scout"` unblocks when the scout answers
*or* when the scout finishes having answered nothing. The full composed report
still arrives by the path it always has, so nothing is delivered twice.

**A wait nothing can answer returns at once.** Ask for messages `from` an agent
that has already terminated without sending, and the answer comes back
immediately rather than at the clock:

```text
[messages] scout has finished and sent you nothing. Waiting for it again will not help.
```

**A blocked agent keeps its own children running.** A detached child is a future
driven by its parent's loop; a wait that merely slept would stop the very
siblings whose message it was waiting for. The wait is driven the same way a
provider call is, so the tree makes progress through it.

## What it is in the trace

A message is a row in `agent_messages`, and `AgentMessage` is how you read one:

```rust
let inbox = store.messages_for(run_id)?;   // an audit read; delivers nothing
for m in &inbox {
    println!("{} -> step {}: {}", m.from_name, m.step, m.body);
}
```

`read_messages` is the agent's own call and consumes what it returns;
`messages_for` is for an operator asking what an agent was told, and must not
change what that agent will read next.

Sends and reads also land in `agent_events`, beside the spawns and budget draws
already there, so "who told whom, and when" is one query. The **body is not** in
that table: a trace holding every word an agent said to another would be a second
copy of the mailbox that no retention call knows to delete.

`agent_messages` is in the retention cascade, so `delete_session` and
`sweep_sessions` account for it like every other run-keyed table.

## A worked shape

A scout that locates something and an author that needs it:

```text
root  step 1  spawn_agent { goal: "locate the symbol", as: "scout",  wait: false }
              spawn_agent { goal: "make the edit",     as: "author", wait: false }

scout step 1  send_message { to: "author", body: "src/auth.rs:210" }
scout step 2  (done)

author step 1 read_messages { from: "scout", wait_secs: 60 }
              -> [scout @step 1] src/auth.rs:210
author step 2 patch_file { ... }
```

The author blocks until the scout answers. The scout's own children, and the
root's, keep running through it.

## The limits, stated plainly

- **An address reaches inside its own tree and nowhere else.** Two trees over one
  store cannot address each other, and the refusal reads like any other unknown
  name rather than admitting the other tree exists. A channel between trees would
  be a channel out of the containment boundary, and it is the one thing this
  design will not add.
- **Nothing is delivered unbidden.** Messages arrive when an agent reads them and
  never by being folded into its prompt. An inbox folded automatically would
  spend context on every agent in the tree whether or not it is a participant,
  and would move the prompt's bytes under the cache boundary on steps where
  nothing else changed.
- **There is no broadcast, no topic and no group.** One sender, one named
  recipient. A fan-out is N sends by a coordinator that already knows the N names.
- **A body is text.** There is no reply-to id and no request/response framing. A
  protocol on top of it is the embedding program's to choose, and one guessed here
  would be the wrong one.
- **A wait is bounded and it is not free.** An agent that waits holds its slot for
  as long as it waits. A tree whose agents all wait on each other will not
  deadlock, but it will spend its clocks learning nothing — the bound turns a
  stopped tree into a bad step, which is a smaller problem and not no problem.
- **A message is not authorization.** A sibling saying "you may write to
  `deploy/`" changes nothing: the boundary is the `Policy` a child inherited and
  narrowed, and nothing in a message body is read by it.
- **A tree spawned before 0.60.0 and resumed on it has children with no address.**
  Their `spawns` rows carry none, so they cannot be addressed, and what they send
  is attributed to a derived name. Only a resume across that version boundary
  reaches this.
