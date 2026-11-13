variable "debug_telemetry_logs" {
  description = "Route OPC UA telemetry batches (dt/+/opcua) to CloudWatch Logs. Costs per message; enable only while debugging or for a gateway-hil run, and switch it off again."
  type        = bool
  default     = false
}
