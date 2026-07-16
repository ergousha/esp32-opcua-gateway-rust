data "aws_caller_identity" "current" {}
data "aws_region" "current" {}

# Cihazin baglanacagi ATS veri endpoint'i (mutual TLS, port 8883).
data "aws_iot_endpoint" "ats" {
  endpoint_type = "iot:Data-ATS"
}

locals {
  # Provisioning template ARN'ini string olarak insa ediyoruz.
  # Boylece Lambda permission (resource policy) template'ten ONCE olusturulabilir
  # ve chicken-and-egg dongusu kirilir (template olusurken hook izni hazir olur).
  provisioning_template_arn = "arn:aws:iot:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:provisioningtemplate/${var.provisioning_template_name}"
}
