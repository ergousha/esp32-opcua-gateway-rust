data "aws_caller_identity" "current" {}
data "aws_region" "current" {}

# ATS data endpoint the device will connect to (mutual TLS, port 8883).
data "aws_iot_endpoint" "ats" {
  endpoint_type = "iot:Data-ATS"
}

locals {
  # Constructing the provisioning template ARN as a string.
  # This allows Lambda permission (resource policy) to be created BEFORE the template itself,
  # breaking the chicken-and-egg loop (so hook permission is ready when the template is created).
  provisioning_template_arn = "arn:aws:iot:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:provisioningtemplate/${var.provisioning_template_name}"
}
