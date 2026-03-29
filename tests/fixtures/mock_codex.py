import json
import sys


def write_json(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()


def handle_exec(argv):
    prompt = sys.stdin.read().strip()
    output_schema = None
    thread_id = "thread-new"
    effort = None

    for index, arg in enumerate(argv):
        if arg == "--output-schema" and index + 1 < len(argv):
            output_schema = argv[index + 1]
        if arg == "resume" and index + 1 < len(argv):
            thread_id = argv[index + 1]
        if arg.startswith("model_reasoning_effort="):
            effort = json.loads(arg.split("=", 1)[1])

    text = f"mock exec: {prompt or 'empty prompt'}"
    if effort:
        text += f" [effort={effort}]"
    if output_schema:
        text = json.dumps({"message": "mock json", "prompt": prompt or ""})

    write_json({"type": "thread.started", "thread_id": thread_id})
    write_json(
        {
            "type": "item.completed",
            "item": {
                "type": "agent_message",
                "id": "item-1",
                "text": text,
            },
        }
    )
    write_json(
        {
            "type": "turn.completed",
            "usage": {
                "input_tokens": 120,
                "cached_input_tokens": 20,
                "output_tokens": 30,
            },
        }
    )


def handle_app_server():
    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        message = json.loads(raw)
        method = message.get("method")
        request_id = message.get("id")
        params = message.get("params", {})

        if method == "initialize":
            write_json(
                {
                    "id": request_id,
                    "result": {
                        "userAgent": "mock-codex",
                    },
                }
            )
        elif method == "initialized":
            continue
        elif method == "model/list":
            write_json(
                {
                    "id": request_id,
                    "result": {
                        "data": [
                            {
                                "id": "gpt-5.4",
                                "model": "gpt-5.4",
                                "displayName": "GPT-5.4",
                                "description": "Mock Codex model",
                                "hidden": False,
                                "isDefault": True,
                                "defaultReasoningEffort": "medium",
                            }
                        ],
                        "nextCursor": None,
                    },
                }
            )
        elif method == "account/read":
            write_json(
                {
                    "id": request_id,
                    "result": {
                        "account": {
                            "type": "apiKey",
                        },
                        "requiresOpenaiAuth": False,
                    },
                }
            )
        elif method == "thread/start":
            write_json(
                {
                    "id": request_id,
                    "result": {
                        "thread": {
                            "id": "thread-stream-new",
                        }
                    },
                }
            )
        elif method == "thread/resume":
            write_json(
                {
                    "id": request_id,
                    "result": {
                        "thread": {
                            "id": params["threadId"],
                        }
                    },
                }
            )
        elif method == "turn/start":
            thread_id = params["threadId"]
            prompt = params.get("input", [{}])[0].get("text", "")
            text = f"streamed mock: {prompt}"
            effort = params.get("effort")
            if effort:
                text += f" [effort={effort}]"
            if params.get("outputSchema") is not None:
                text = json.dumps({"message": "streamed mock", "prompt": prompt})

            write_json({"id": request_id, "result": {"turn": {"id": "turn-1", "status": "inProgress"}}})
            write_json(
                {
                    "method": "thread/tokenUsage/updated",
                    "params": {
                        "threadId": thread_id,
                        "turnId": "turn-1",
                        "tokenUsage": {
                            "last": {
                                "inputTokens": 140,
                                "cachedInputTokens": 40,
                                "outputTokens": 33,
                                "reasoningOutputTokens": 3,
                                "totalTokens": 173,
                            },
                            "total": {
                                "inputTokens": 140,
                                "cachedInputTokens": 40,
                                "outputTokens": 33,
                                "reasoningOutputTokens": 3,
                                "totalTokens": 173,
                            },
                        },
                    },
                }
            )
            write_json(
                {
                    "method": "item/agentMessage/delta",
                    "params": {
                        "threadId": thread_id,
                        "turnId": "turn-1",
                        "itemId": "item-1",
                        "delta": text,
                    },
                }
            )
            write_json(
                {
                    "method": "turn/completed",
                    "params": {
                        "threadId": thread_id,
                        "turn": {
                            "id": "turn-1",
                            "status": "completed",
                            "items": [],
                        },
                    },
                }
            )
        else:
            write_json(
                {
                    "id": request_id,
                    "error": {
                        "code": -32601,
                        "message": f"Unsupported mock method: {method}",
                    },
                }
            )


def main():
    argv = sys.argv[1:]
    if not argv:
        raise SystemExit(1)

    if argv[0] == "app-server":
        handle_app_server()
        return

    if argv[0] == "exec":
        handle_exec(argv[1:])
        return

    raise SystemExit(f"unsupported mock codex mode: {argv[0]}")


if __name__ == "__main__":
    main()
