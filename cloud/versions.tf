terraform {
  required_version = ">= 1.10.0" # use_lockfile needs 1.10

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 6.71"
    }
  }

  # The platform's state bucket, this product's key (plan §7; the platform
  # publishes both in SSM /iot-platform/products/gateway/platform).
  backend "s3" {
    bucket       = "esp32-ztp-tfstate-637423178579"
    key          = "products/gateway/cloud.tfstate"
    region       = "eu-central-1"
    encrypt      = true
    use_lockfile = true
  }
}

provider "aws" {
  region = "eu-central-1"

  default_tags {
    tags = {
      Product   = "gateway"
      ManagedBy = "terraform"
    }
  }
}
