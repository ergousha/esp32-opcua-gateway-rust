# ---------------------------------------------------------------------------
# Cihaz kayit veritabani.
# PAY_PER_REQUEST = on-demand: bosta dururken maliyet ~0, sadece okunan/yazilan
# istek basina odenir. PoC icin ideal.
# ---------------------------------------------------------------------------
resource "aws_dynamodb_table" "device_registry" {
  name         = "${var.project_name}-device-registry"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "mac_address"

  attribute {
    name = "mac_address"
    type = "S"
  }

  point_in_time_recovery {
    enabled = false # PoC: kapali. Uretimde true yapin.
  }

  server_side_encryption {
    enabled = true # AWS-owned KMS anahtari (ek ucret yok).
  }

  tags = {
    Name = "${var.project_name}-device-registry"
  }
}

# Ornek cihaz kaydi.
# Not: mac_address'i ESP32'nizin gercek MAC'i ile degistirin
# (firmware bootlarken loglar; ya da scripts/seed_device.py kullanin).
# Bu kaydi acik birakmak yerine seed script tercih edilebilir; PoC kolayligi
# icin burada bir ornek tutuluyor.
resource "aws_dynamodb_table_item" "sample_device" {
  table_name = aws_dynamodb_table.device_registry.name
  hash_key   = aws_dynamodb_table.device_registry.hash_key

  item = jsonencode({
    mac_address = { S = "AA:BB:CC:DD:EE:FF" }
    secret      = { S = "change-me-shared-secret" }
    allowed     = { BOOL = true }
    provisioned = { BOOL = false }
    note        = { S = "terraform ornek kaydi - gercek cihazla degistirin" }
  })

  lifecycle {
    # Lambda 'provisioned' alanini gunceller; terraform tekrar ezmesin.
    ignore_changes = [item]
  }
}
