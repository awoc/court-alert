use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub(super) struct GraphQlRequest<'a, V: Serialize> {
    pub query: &'a str,
    pub variables: V,
}

#[derive(Debug, Serialize)]
pub(super) struct BookingSlotsVariables {
    #[serde(rename = "productID")]
    pub product_id: Uuid,
    pub input: BookingSlotsQueryInput,
}

#[derive(Debug, Serialize)]
pub(super) struct BookingSlotsQueryInput {
    #[serde(rename = "start", serialize_with = "serialize_datetime_millis")]
    pub starts_at: DateTime<Utc>,
    #[serde(rename = "end", serialize_with = "serialize_datetime_millis")]
    pub ends_at: DateTime<Utc>,
}

fn serialize_datetime_millis<S: Serializer>(
    datetime: &DateTime<Utc>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&datetime.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

#[derive(Debug, Deserialize)]
pub(super) struct BookingSlotsResponseDto {
    #[serde(default)]
    pub data: Option<BookingSlotsDataDto>,
    #[serde(default)]
    pub errors: Option<Vec<GraphQlErrorDto>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphQlErrorDto {
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct BookingSlotsDataDto {
    pub booking_slots: Vec<BookingSlotDto>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct BookingSlotDto {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    #[serde(default)]
    pub booking_period_end: Option<DateTime<Utc>>,
    pub availability: u32,
    pub already_booked: u32,
    pub already_in_cart: u32,
    pub already_on_waiting_list: u32,
    pub blocked_by_resource: bool,
}
