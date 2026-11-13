# tflint for cloud/. The platform's policies (iot-device-sdk tflint/policies)
# come from the SDK revision Cargo.toml pins: CI sets TFLINT_OPA_POLICY_DIR.
# Locally: TFLINT_OPA_POLICY_DIR=<sdk checkout>/tflint/policies tflint
plugin "terraform" {
  enabled = true
  preset  = "recommended"
}

plugin "aws" {
  enabled = true
  version = "0.32.0"
  source  = "github.com/terraform-linters/tflint-ruleset-aws"
}

plugin "opa" {
  enabled = true
  version = "0.12.0"
  source  = "github.com/terraform-linters/tflint-ruleset-opa"
}
