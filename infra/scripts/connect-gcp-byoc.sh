#!/usr/bin/env bash
# Zero-Terraform BYOC onboarding script for connecting a GCP project to open-ci.
# Creates a least-privilege runner-manager service account, grants keyless
# impersonation to the open-ci control plane SA, configures an egress-only
# runner subnet + Cloud NAT (or reuses an existing subnet), and prints a JSON
# connection bundle ready to paste into the Hosted open-ci onboarding wizard.

set -euo pipefail

PROJECT_ID=""
REGION="us-central1"
CONTROL_PLANE_SA=""
NAME_PREFIX="open-ci"
SUBNET_CIDR="10.44.0.0/20"
EXISTING_NETWORK=""
EXISTING_SUBNET=""
ENABLE_CACHE_BUCKET=0
DRY_RUN=0

usage() {
  cat <<'EOF'
Usage: connect-gcp-byoc.sh --project <PROJECT_ID> --control-plane-sa <SA_EMAIL> [options]

Required:
  --project <id>              Customer GCP project ID
  --control-plane-sa <email>  Hosted (or remote) open-ci control plane service account email

Optional:
  --region <region>           GCP region (default: us-central1)
  --prefix <name>             Resource prefix (default: open-ci)
  --subnet-cidr <cidr>        CIDR for dedicated runner subnet (default: 10.44.0.0/20)
  --existing-network <name>   Existing VPC network name (skips VPC/NAT creation)
  --existing-subnet <path>    Existing subnetwork path (projects/<p>/regions/<r>/subnetworks/<n>)
  --enable-cache-bucket       Create a regional GCS bucket for direct BYOC Actions caching
  --dry-run                   Print gcloud commands without applying changes
  -h, --help                  Show this help message
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --project)
      PROJECT_ID="${2:-}"
      shift 2
      ;;
    --region)
      REGION="${2:-}"
      shift 2
      ;;
    --control-plane-sa)
      CONTROL_PLANE_SA="${2:-}"
      shift 2
      ;;
    --prefix)
      NAME_PREFIX="${2:-}"
      shift 2
      ;;
    --subnet-cidr)
      SUBNET_CIDR="${2:-}"
      shift 2
      ;;
    --existing-network)
      EXISTING_NETWORK="${2:-}"
      shift 2
      ;;
    --existing-subnet)
      EXISTING_SUBNET="${2:-}"
      shift 2
      ;;
    --enable-cache-bucket)
      ENABLE_CACHE_BUCKET=1
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'ERROR: unknown option: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z "$PROJECT_ID" || -z "$CONTROL_PLANE_SA" ]]; then
  printf 'ERROR: --project and --control-plane-sa are required.\n' >&2
  usage >&2
  exit 2
fi

if [[ ! "$NAME_PREFIX" =~ ^[a-z][a-z0-9-]{1,20}$ ]]; then
  printf 'ERROR: --prefix must match ^[a-z][a-z0-9-]{1,20}$\n' >&2
  exit 2
fi

if [[ ( -n "$EXISTING_NETWORK" && -z "$EXISTING_SUBNET" ) || ( -z "$EXISTING_NETWORK" && -n "$EXISTING_SUBNET" ) ]]; then
  printf 'ERROR: --existing-network and --existing-subnet must be provided together.\n' >&2
  exit 2
fi

if [[ "$DRY_RUN" -eq 0 ]] && ! command -v gcloud >/dev/null 2>&1; then
  printf 'ERROR: gcloud CLI not found on PATH.\n' >&2
  exit 1
fi

run_cmd() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    {
      printf '[dry-run]'
      printf ' %q' "$@"
      printf '\n'
    } >&2
  else
    "$@"
  fi
}

SA_ID="${NAME_PREFIX}-byoc"
SA_EMAIL="${SA_ID}@${PROJECT_ID}.iam.gserviceaccount.com"
ROLE_PREFIX="${NAME_PREFIX//-/_}"
PROVISIONER_ROLE_ID="${ROLE_PREFIX}_byoc_provisioner"
CLEANER_ROLE_ID="${ROLE_PREFIX}_byoc_cleaner"
RUNNER_TAG="${NAME_PREFIX}-runner"
VPC_NAME="${NAME_PREFIX}-runners-vpc"
SUBNET_NAME="${NAME_PREFIX}-runners-${REGION}"
ROUTER_NAME="${NAME_PREFIX}-nat-router-${REGION}"
NAT_NAME="${NAME_PREFIX}-nat-${REGION}"
FW_NAME="${NAME_PREFIX}-deny-runner-ingress"
FW_PRIORITY=100
CACHE_BUCKET="${PROJECT_ID}-${NAME_PREFIX}-cache-${REGION}"

PROVISIONER_PERMS="compute.disks.create,compute.disks.get,compute.disks.list,compute.disks.setLabels,compute.disks.use,compute.images.get,compute.images.getFromFamily,compute.images.useReadOnly,compute.instances.create,compute.instances.get,compute.instances.list,compute.instances.setLabels,compute.instances.setMetadata,compute.instances.setTags,compute.networks.list,compute.regions.get,compute.subnetworks.list,compute.subnetworks.use,compute.subnetworks.useExternalIp,compute.zoneOperations.get,compute.zones.get"
CLEANER_PERMS="compute.disks.delete,compute.instances.delete"
CLEANER_CONDITION="expression=resource.name.extract('/instances/{name}').startsWith('open-ci-') || resource.name.extract('/disks/{name}').startsWith('open-ci-'),title=OnlyOpenCiPrefixedResources,description=Restrict VM and disk deletion to open-ci-* ephemeral runners."

printf '==> Enabling required GCP APIs in %s\n' "$PROJECT_ID"
run_cmd gcloud services enable compute.googleapis.com iam.googleapis.com iamcredentials.googleapis.com --project="$PROJECT_ID" --quiet

printf '==> Ensuring BYOC runner-manager service account (%s)\n' "$SA_EMAIL"
if [[ "$DRY_RUN" -eq 1 ]] || ! gcloud iam service-accounts describe "$SA_EMAIL" --project="$PROJECT_ID" >/dev/null 2>&1; then
  run_cmd gcloud iam service-accounts create "$SA_ID" \
    --project="$PROJECT_ID" \
    --display-name="open-ci BYOC Runner Manager" \
    --description="Impersonated keylessly by open-ci to launch zero-IAM ephemeral Spot runners."
fi

printf '==> Granting keyless impersonation to %s\n' "$CONTROL_PLANE_SA"
run_cmd gcloud iam service-accounts add-iam-policy-binding "$SA_EMAIL" \
  --project="$PROJECT_ID" \
  --member="serviceAccount:${CONTROL_PLANE_SA}" \
  --role="roles/iam.serviceAccountTokenCreator" \
  --quiet >/dev/null

printf '==> Ensuring least-privilege custom IAM roles (zero iam.serviceAccounts.actAs)\n'
if [[ "$DRY_RUN" -eq 1 ]] || ! gcloud iam roles describe "$PROVISIONER_ROLE_ID" --project="$PROJECT_ID" >/dev/null 2>&1; then
  run_cmd gcloud iam roles create "$PROVISIONER_ROLE_ID" \
    --project="$PROJECT_ID" \
    --title="open-ci BYOC Runner Provisioner" \
    --description="Least-privilege Compute Engine permissions without iam.serviceAccounts.actAs." \
    --permissions="$PROVISIONER_PERMS" \
    --stage=GA \
    --quiet >/dev/null
else
  run_cmd gcloud iam roles update "$PROVISIONER_ROLE_ID" \
    --project="$PROJECT_ID" \
    --permissions="$PROVISIONER_PERMS" \
    --quiet >/dev/null
fi

if [[ "$DRY_RUN" -eq 1 ]] || ! gcloud iam roles describe "$CLEANER_ROLE_ID" --project="$PROJECT_ID" >/dev/null 2>&1; then
  run_cmd gcloud iam roles create "$CLEANER_ROLE_ID" \
    --project="$PROJECT_ID" \
    --title="open-ci BYOC Runner Cleaner" \
    --description="Scoped delete permissions bound to open-ci-* ephemeral instances and disks." \
    --permissions="$CLEANER_PERMS" \
    --stage=GA \
    --quiet >/dev/null
else
  run_cmd gcloud iam roles update "$CLEANER_ROLE_ID" \
    --project="$PROJECT_ID" \
    --permissions="$CLEANER_PERMS" \
    --quiet >/dev/null
fi

run_cmd gcloud projects add-iam-policy-binding "$PROJECT_ID" \
  --member="serviceAccount:${SA_EMAIL}" \
  --role="projects/${PROJECT_ID}/roles/${PROVISIONER_ROLE_ID}" \
  --condition=None \
  --quiet >/dev/null

run_cmd gcloud projects add-iam-policy-binding "$PROJECT_ID" \
  --member="serviceAccount:${SA_EMAIL}" \
  --role="projects/${PROJECT_ID}/roles/${CLEANER_ROLE_ID}" \
  --condition="$CLEANER_CONDITION" \
  --quiet >/dev/null

if [[ -n "$EXISTING_NETWORK" ]]; then
  FINAL_NETWORK="$EXISTING_NETWORK"
  FINAL_SUBNET="$EXISTING_SUBNET"
else
  FINAL_NETWORK="$VPC_NAME"
  FINAL_SUBNET="projects/${PROJECT_ID}/regions/${REGION}/subnetworks/${SUBNET_NAME}"

  printf '==> Ensuring dedicated egress-only runner VPC, subnet, and Cloud NAT\n'
  if [[ "$DRY_RUN" -eq 1 ]] || ! gcloud compute networks describe "$VPC_NAME" --project="$PROJECT_ID" >/dev/null 2>&1; then
    run_cmd gcloud compute networks create "$VPC_NAME" \
      --project="$PROJECT_ID" \
      --subnet-mode=custom \
      --quiet >/dev/null
  fi

  if [[ "$DRY_RUN" -eq 1 ]] || ! gcloud compute networks subnets describe "$SUBNET_NAME" --region="$REGION" --project="$PROJECT_ID" >/dev/null 2>&1; then
    run_cmd gcloud compute networks subnets create "$SUBNET_NAME" \
      --project="$PROJECT_ID" \
      --region="$REGION" \
      --network="$VPC_NAME" \
      --range="$SUBNET_CIDR" \
      --enable-private-ip-google-access \
      --quiet >/dev/null
  fi

  if [[ "$DRY_RUN" -eq 1 ]] || ! gcloud compute routers describe "$ROUTER_NAME" --region="$REGION" --project="$PROJECT_ID" >/dev/null 2>&1; then
    run_cmd gcloud compute routers create "$ROUTER_NAME" \
      --project="$PROJECT_ID" \
      --region="$REGION" \
      --network="$VPC_NAME" \
      --quiet >/dev/null
  fi

  if [[ "$DRY_RUN" -eq 1 ]] || ! gcloud compute routers nats describe "$NAT_NAME" --router="$ROUTER_NAME" --region="$REGION" --project="$PROJECT_ID" >/dev/null 2>&1; then
    run_cmd gcloud compute routers nats create "$NAT_NAME" \
      --project="$PROJECT_ID" \
      --region="$REGION" \
      --router="$ROUTER_NAME" \
      --auto-allocate-nat-external-ips \
      --nat-custom-subnet-ip-ranges="$SUBNET_NAME" \
      --enable-logging \
      --log-filter=ERRORS_ONLY \
      --quiet >/dev/null
  fi
fi

printf '==> Ensuring deny-all-ingress firewall rule for runner tag (%s) at priority %s\n' "$RUNNER_TAG" "$FW_PRIORITY"
if [[ "$DRY_RUN" -eq 1 ]] || ! gcloud compute firewall-rules describe "$FW_NAME" --project="$PROJECT_ID" >/dev/null 2>&1; then
  run_cmd gcloud compute firewall-rules create "$FW_NAME" \
    --project="$PROJECT_ID" \
    --network="$FINAL_NETWORK" \
    --direction=INGRESS \
    --priority="$FW_PRIORITY" \
    --action=DENY \
    --rules=all \
    --source-ranges=0.0.0.0/0 \
    --target-tags="$RUNNER_TAG" \
    --quiet >/dev/null
else
  run_cmd gcloud compute firewall-rules update "$FW_NAME" \
    --project="$PROJECT_ID" \
    --priority="$FW_PRIORITY" \
    --source-ranges=0.0.0.0/0 \
    --target-tags="$RUNNER_TAG" \
    --quiet >/dev/null
fi

CACHE_BUCKET_JSON="null"
if [[ "$ENABLE_CACHE_BUCKET" -eq 1 ]]; then
  printf '==> Ensuring regional GCS cache bucket (%s)\n' "$CACHE_BUCKET"
  if [[ "$DRY_RUN" -eq 1 ]] || ! gcloud storage buckets describe "gs://${CACHE_BUCKET}" --project="$PROJECT_ID" >/dev/null 2>&1; then
    run_cmd gcloud storage buckets create "gs://${CACHE_BUCKET}" \
      --project="$PROJECT_ID" \
      --location="$REGION" \
      --uniform-bucket-level-access \
      --public-access-prevention \
      --quiet >/dev/null
  fi
  run_cmd gcloud storage buckets add-iam-policy-binding "gs://${CACHE_BUCKET}" \
    --member="serviceAccount:${SA_EMAIL}" \
    --role="roles/storage.objectAdmin" \
    --quiet >/dev/null
  CACHE_BUCKET_JSON="\"${CACHE_BUCKET}\""
fi

printf '\n==> GCP BYOC connection bundle (paste into open-ci onboarding):\n'
cat <<EOF
{
  "provider": "gcp",
  "project_id": "${PROJECT_ID}",
  "region": "${REGION}",
  "service_account_email": "${SA_EMAIL}",
  "network": "${FINAL_NETWORK}",
  "subnetwork": "${FINAL_SUBNET}",
  "network_tag": "${RUNNER_TAG}",
  "cache_bucket": ${CACHE_BUCKET_JSON}
}
EOF
