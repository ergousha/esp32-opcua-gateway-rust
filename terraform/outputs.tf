output "iot_endpoint" {
  description = "Firmware'de CONFIG_MQTT_BROKER_ENDPOINT olarak kullanin (port 8883)."
  value       = data.aws_iot_endpoint.ats.endpoint_address
}

output "provisioning_template_name" {
  description = "Firmware'deki template adiyla ayni olmali."
  value       = aws_iot_provisioning_template.fleet.name
}

output "dynamodb_table" {
  value = aws_dynamodb_table.device_registry.name
}

output "claim_certificate_pem" {
  description = "Cihaza flashlanacak CLAIM sertifikasi (certs/claim.crt.pem)."
  value       = aws_iot_certificate.claim.certificate_pem
  sensitive   = true
}

output "claim_private_key" {
  description = "Cihaza flashlanacak CLAIM private key (certs/claim.private.key)."
  value       = aws_iot_certificate.claim.private_key
  sensitive   = true
}

output "claim_public_key" {
  value     = aws_iot_certificate.claim.public_key
  sensitive = true
}

# ---------------------------------------------------------------------------
# Sertifikalari dosyaya cikarmak icin (sensitive oldugundan ekrana basilmaz):
#
#   terraform output -raw claim_certificate_pem > ../firmware/certs/claim.crt.pem
#   terraform output -raw claim_private_key     > ../firmware/certs/claim.private.key
#
# Amazon Root CA 1:
#   curl -s https://www.amazontrust.com/repository/AmazonRootCA1.pem \
#     > ../firmware/certs/AmazonRootCA1.pem
# ---------------------------------------------------------------------------
