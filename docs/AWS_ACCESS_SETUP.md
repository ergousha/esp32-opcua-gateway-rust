# AWS Account Access + Terraform Deployment

This guide explains step-by-step **how to access AWS** and **how to run Terraform** to deploy the PoC. Proceed in order.

---

## 1. Required Tools

```sh
# macOS (brew)
brew install terraform awscli
terraform version   # >= 1.5
aws --version       # v2
```

Additionally, `python3` is required for packaging the Lambda (the zip package is automatically created via the Terraform `archive_provider`; no extra steps are required).

---

## 2. AWS Account Access (Personal Account — IAM User Access Key)

You are using a personal AWS account. Do not do daily work with the **Root** user; instead, create an **IAM user** and work with its access key.

### 2.1 Create an IAM User (Once, from the Console)

1. AWS Console → **IAM** → **Users** → **Create user** (e.g., `esp32-ztp-admin`).
2. Permissions: Add the **AdministratorAccess** managed policy for the PoC (simplest approach; see below for a restricted set).
3. Once the user is created → **Security credentials** → **Create access key** → select **Command Line Interface (CLI)** as the type → save the resulting **Access key ID** + **Secret access key** pair (the secret will not be displayed again).

> MFA should be enabled on the root account; MFA is also recommended for the IAM user.

### 2.2 Configure the CLI

```sh
aws configure --profile esp32-ztp
# AWS Access Key ID:      <access key id>
# AWS Secret Access Key:  <secret access key>
# Default region name:    eu-central-1
# Default output format:  json
```

Select the profile and verify the identity in each terminal:

```sh
export AWS_PROFILE=esp32-ztp
aws sts get-caller-identity   # the created IAM user should appear in the ARN
```

> Credentials are kept under `~/.aws/credentials`; never commit this file. If an access key leaks, deactivate/delete it from the console immediately.

**Alternative — Env File:** Instead of `aws configure`, you can load credentials into the shell using env variables. An `aws-env.sh.example` template is provided in the repository root:

```sh
cp aws-env.sh.example aws-env.sh   # fill in actual access key/secret
source ./aws-env.sh                # in every new terminal
aws sts get-caller-identity        # verify
```

`aws-env.sh` is in `.gitignore` — it contains the actual secret, so it is not committed.

### Required IAM Permissions

Terraform creates resources in these services; your IAM user must be authorized for them (for PoC, `AdministratorAccess` is the easiest, but the restricted set below is also sufficient):

- `iot:*` (Fleet Provisioning template, policy, certificate, topic rule)
- `dynamodb:*` (table)
- `lambda:*` (hook function)
- `iam:*` (role/policy — Terraform creates roles; `iam:PassRole` is required)
- `logs:*` (CloudWatch log groups)

---

## 3. (Optional but Recommended) Remote Terraform State

A local state is sufficient for PoC. For teams/repeatability, S3 backend is used:

```sh
# Once: create the state bucket (versioning + encryption enabled)
aws s3api create-bucket --bucket <company>-esp32-ztp-tfstate \
  --region eu-central-1 --create-bucket-configuration LocationConstraint=eu-central-1
aws s3api put-bucket-versioning --bucket <company>-esp32-ztp-tfstate \
  --versioning-configuration Status=Enabled
aws s3api put-bucket-encryption --bucket <company>-esp32-ztp-tfstate \
  --server-side-encryption-configuration '{"Rules":[{"ApplyServerSideEncryptionByDefault":{"SSEAlgorithm":"AES256"}}]}'
```

Then uncomment the `backend "s3"` block in `terraform/versions.tf` (write the bucket name) and run `terraform init -migrate-state`. For locking in modern Terraform, `use_lockfile = true` (S3 native lock) is sufficient — no separate DynamoDB lock table is required.

---

## 4. Deploy with Terraform

```sh
cd terraform
cp terraform.tfvars.example terraform.tfvars   # edit if necessary (region, etc.)

terraform init
terraform plan     # examine what will be created
terraform apply    # approve
```

Important outputs produced:

```sh
terraform output iot_endpoint                # firmware config::MQTT_ENDPOINT
terraform output provisioning_template_name  # firmware config::PROVISIONING_TEMPLATE
terraform output dynamodb_table              # seed script table name
```

Extract claim certificates into firmware:

```sh
terraform output -raw claim_certificate_pem > ../certs/claim.crt.pem
terraform output -raw claim_private_key     > ../certs/claim.private.key
```

---

## 5. Cost

All are serverless + on-demand; **~0 USD when idle**:

| Service | Model | PoC Cost |
| --- | --- | --- |
| IoT Core | per message/connection | thousands of messages = a few cents |
| DynamoDB | PAY_PER_REQUEST | per read/write; negligible in PoC |
| Lambda | request + GB-sec | 1 short invocation per provisioning; free tier |
| CloudWatch Logs | GB + storage | 7-day retention; minimal |

> On first provisioning, AWS generates a device certificate; the certificate itself is free. The actual fee depends on the MQTT message volume.

---

## 6. Teardown

```sh
cd terraform
terraform destroy
```

> Note: `terraform destroy` does not delete certificates/things **later created by the devices** via provisioning (Terraform does not manage them). Clean them up from the IoT Core console or with `aws iot delete-thing` / `delete-certificate`. The claim certificate and template are deleted by Terraform.
