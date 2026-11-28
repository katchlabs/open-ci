# BYOC Onboarding & Compute Mode Presets

`open-ci` supports both 100% self-hosted deployments and **Bring Your Own Cloud (BYOC)** connections where a remote or hosted `open-ci` control plane launches ephemeral zero-IAM runners directly inside your GCP project and/or AWS account.

## Choose Your Compute Mode

| Mode | Preset Config | Best For | How It Works |
| --- | --- | --- | --- |
| **Spot Only (BYOC AWS / GCP)** | [`config/open-ci.high-perf.example.toml`](../config/open-ci.high-perf.example.toml) | Zero fixed compute cost; pay-per-second Spot VMs with local NVMe SSDs | Launches ephemeral Spot VMs (`c4d`/`c4a` on GCP, `c7ad`/`c8gd` on AWS) directly in your cloud account with automatic AZ/provider failover and optional warm idle pool. |
| **Hybrid (Bare-Metal Baseline + BYOC Spot Burst)** | [`config/open-ci.hybrid-byoc.example.toml`](../config/open-ci.hybrid-byoc.example.toml) | Sub-second 5.7 GHz baseline performance with unlimited Spot burst capacity | Places `cloud_hypervisor` bare-metal microVMs first (`prefer_cheapest = true`, no `provider_weights`) and automatically spills parallel bursts onto your GCP/AWS Spot placements with zero workflow YAML changes. |

## Connect GCP (Zero Controller VM)

Your GCP project grants short-lived keyless **Service Account Impersonation** (`roles/iam.serviceAccountTokenCreator`) to the control plane service account. Ephemeral runners launch with `"serviceAccounts": []`, IMDS blocked for UID `runner`, and a GCP IAM Condition restricting VM/disk deletion exclusively to `open-ci-*` resources.

### Option A: One-Liner (`gcloud`, No Terraform)

```bash
./infra/scripts/connect-gcp-byoc.sh \
  --project YOUR_GCP_PROJECT_ID \
  --region us-central1 \
  --control-plane-sa CONTROL_PLANE_SA_EMAIL
```

Add `--enable-cache-bucket` to also provision a private regional GCS bucket for direct BYOC Actions caching, or pass `--existing-network` and `--existing-subnet` to reuse an existing VPC. The script prints a JSON connection bundle ready to paste into your control plane.

The provisioner role includes `compute.subnetworks.useExternalIp`, so runners with `external_ipv4 = true` (the `open-ci init` GCP default) egress directly instead of through Cloud NAT, while the `open-ci-deny-runner-ingress` firewall on tag `open-ci-runner` (priority `100`, above typical `allow` rules in reused VPCs) keeps every inbound port closed. Re-running the script updates an existing role and firewall priority in place. Runners boot the public `open-ci-public-images` images by default, so no Packer run is needed.

### Option B: Terraform (`infra/terraform/modules/gcp-byoc`)

```hcl
module "open_ci_gcp_byoc" {
  source = "github.com/katchlabs/open-ci//infra/terraform/modules/gcp-byoc"

  project_id                    = "your-gcp-project-id"
  region                        = "us-central1"
  hosted_control_plane_sa_email = "CONTROL_PLANE_SA_EMAIL"
  create_cache_bucket           = false
}

output "open_ci_connection_bundle" {
  value = module.open_ci_gcp_byoc.connection_bundle
}
```

## Connect AWS (Zero Controller VM)

Your AWS account creates a cross-account IAM Role requiring a unique high-entropy `sts:ExternalId`. The IAM policy enforces `aws:RequestTag/open-ci-managed = "true"`, `ec2:MetadataHttpTokens = "required"` (IMDSv2), tag-scoped termination, and zero runner IAM instance profiles.

### Option A: 1-Click CloudFormation Quick-Create

Deploy [`infra/cloudformation/open-ci-aws-byoc.yaml`](../infra/cloudformation/open-ci-aws-byoc.yaml) in the target region with your `TrustedPrincipalArn`, `ExternalId`, `VpcId`, and `SubnetIds`, then copy the `ConnectionBundleJson` stack output.

### Option B: Terraform (`infra/terraform/modules/aws-byoc`)

```hcl
module "open_ci_aws_byoc" {
  source = "github.com/katchlabs/open-ci//infra/terraform/modules/aws-byoc"

  aws_region            = "us-east-1"
  trusted_principal_arn = "arn:aws:iam::CONTROL_PLANE_ACCOUNT:role/open-ci-hub"
  external_id           = "YOUR_GENERATED_EXTERNAL_ID"
  create_cache_bucket   = false
}

output "open_ci_connection_bundle" {
  value = module.open_ci_aws_byoc.connection_bundle
}
```

## Security Guarantees

- **Zero code or secrets on control-plane disks**: Source checkouts, build artifacts, and GitHub secrets stay strictly inside your VPC.
- **Zero cloud IAM on runners**: GCP enforces `"serviceAccounts": []` (and omits `iam.serviceAccounts.actAs`); AWS omits `iam:PassRole` and enforces IMDSv2 at both API and IAM layers; `open-ci agent bootstrap` additionally blocks UID `runner` from `169.254.169.254`.
- **Single-use bootstrap & triple-layer expiry**: Bootstrap tokens are consumed atomically once (`WHERE bootstrap_used = 0`), and every VM carries tenant/expiry metadata (`open-ci-tenant`, `open-ci-expires-at`) enforced by the worker agent, reconciler, and cloud janitor.
