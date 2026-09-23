# Court alerts

Court alerts connect bookable court slots with people who want to play.

## Language

**Booking provider**:
A source of court availability for a venue, such as ZHS or Playtomic.
_Avoid_: Provider without a qualifier when discussing chat delivery

**Chat provider**:
A messaging platform through which people receive court alerts, such as Discord or a future Telegram integration.
_Avoid_: Discord as a synonym for all chat delivery

**Alert message**:
A message announcing bookable court slots through a chat provider. Its meaning is independent of the provider's presentation and delivery conventions.

**Alert destination**:
The conversation or channel in which an alert message lives. A message's identity includes its chat provider and destination because different conversations or providers can reuse the same message ID.

**Alert message lifecycle**:
Tracking a delivered alert, marking its slots when they stop being bookable, and forgetting it when it is deleted or all its slots have ended. This policy is shared across chat providers.
