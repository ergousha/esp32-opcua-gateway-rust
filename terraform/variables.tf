variable "aws_region" {
  description = "AWS bolgesi (IoT Core desteklenen bir bolge secin)."
  type        = string
  default     = "eu-central-1"
}

variable "project_name" {
  description = "Kaynak isimlendirme icin prefix."
  type        = string
  default     = "esp32-ztp"
}

variable "provisioning_template_name" {
  description = "Fleet Provisioning template adi. Firmware ile ayni olmali."
  type        = string
  default     = "esp32-s3-fleet-template"
}

variable "log_retention_days" {
  description = "CloudWatch log saklama suresi (maliyet icin dusuk tutuldu)."
  type        = number
  default     = 7
}

variable "telemetry_topic_prefix" {
  description = "Cihazlarin publish edecegi telemetri topic prefix'i."
  type        = string
  default     = "dt"
}
