# Court alerts

Court alerts connect bookable court slots with people who want to play.

## Language

**Booking provider**:
A source of court availability for a venue, such as ZHS or Playtomic.
_Avoid_: Provider without a qualifier when discussing chat delivery

**Integration**:
A system through which people manage subscriptions and receive the alerts that match them, such as Discord or the webhook API used by agents.
_Avoid_: Provider without a qualifier, subscriber channel

**Chat provider**:
An integration that is a messaging platform, such as Discord or a future Telegram integration.
_Avoid_: Discord as a synonym for all chat delivery

**Webhook integration**:
An integration in which an external system, such as an agent, manages subscriptions over HTTP and receives matching alerts as signed HTTP requests. Its alerts are delivered once and never updated afterwards.

**Subscriber**:
The person who owns a subscription, identified by an ID that is only meaningful within one integration. For the webhook integration, the calling system chooses the ID and may act for several people.

**Alert message**:
A message announcing bookable court slots through a chat provider. Its meaning is independent of the chat provider's presentation and delivery conventions.

**Alert destination**:
The conversation or channel in which an alert message lives. A message's identity includes its chat provider and destination because different conversations or chat providers can reuse the same message ID.

**Alert message lifecycle**:
Tracking a delivered alert, marking its slots when they stop being bookable, and forgetting it when it is deleted or all its slots have ended. This policy is shared across chat providers.
