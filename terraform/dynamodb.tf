# ---------------------------------------------------------------------------
# Device registry database.
# PAY_PER_REQUEST = on-demand: cost is ~0 when idle, you only pay per
# read/write request. Ideal for PoC.
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
    enabled = false # PoC: disabled. Set to true in production.
  }

  server_side_encryption {
    enabled = true # AWS-owned KMS key (no additional charge).
  }

  tags = {
    Name = "${var.project_name}-device-registry"
  }
}

# Example device record.
# Note: replace the mac_address with your ESP32's actual MAC address
# (the firmware logs it on boot; or use scripts/seed_device.py).
# A seed script can be preferred instead of leaving this open; an example
# is kept here for PoC convenience.
resource "aws_dynamodb_table_item" "sample_device" {
  table_name = aws_dynamodb_table.device_registry.name
  hash_key   = aws_dynamodb_table.device_registry.hash_key

  item = jsonencode({
    mac_address = { S = "AA:BB:CC:DD:EE:FF" }
    secret      = { S = "change-me-shared-secret" }
    allowed     = { BOOL = true }
    provisioned = { BOOL = false }
    note        = { S = "terraform example record - replace with actual device" }
  })

  lifecycle {
    # Lambda updates the 'provisioned' field; prevent terraform from overwriting it.
    ignore_changes = [item]
  }
}
