terraform {
  required_version = ">= 1.5.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.60"
    }
    archive = {
      source  = "hashicorp/archive"
      version = "~> 2.4"
    }
  }

  # PoC icin local state. Ekip/uretim icin asagidaki S3 backend'i acin
  # (bkz. docs/AWS_ACCESS_SETUP.md).
  #
  # backend "s3" {
  #   bucket       = "<tfstate-bucket>"
  #   key          = "esp32-ztp/terraform.tfstate"
  #   region       = "eu-central-1"
  #   encrypt      = true
  #   use_lockfile = true
  # }
}
