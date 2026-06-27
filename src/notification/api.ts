// Notification composition adapter (#1460). Wraps the send_notification Tauri command so callers
// reuse the generated request/response contracts instead of spelling command names or camelCase
// payload fields by hand.
import { getTransport } from "../transport";
import type { SendNotificationRequest, SendNotificationResponse } from "../types";

export function sendNotification(
  request: SendNotificationRequest,
): Promise<SendNotificationResponse> {
  return getTransport().request<SendNotificationResponse>("send_notification", { request });
}
