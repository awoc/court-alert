import json
import sys

alert = json.load(sys.stdin)
alert["chat_id"] = alert["user_id"].partition(":")[2]
alert["slot_lines"] = "\n".join(
    f"- {slot['club']}, {slot['court']}: {slot['starts_at']} to {slot['ends_at']}"
    for slot in alert["slots"]
)
print(json.dumps(alert))
