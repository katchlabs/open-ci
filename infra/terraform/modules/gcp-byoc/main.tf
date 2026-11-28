locals {
  common_labels = merge(
    {
      "open-ci-managed" = "true"
      "open-ci-role"    = "byoc"
    },
    var.labels,
  )
  provisioner_role_id = replace("${var.name_prefix}_byoc_provisioner", "-", "_")
  cleaner_role_id     = replace("${var.name_prefix}_byoc_cleaner", "-", "_")
  runner_network_tag  = "${var.name_prefix}-runner"
  network_name        = var.create_network ? google_compute_network.runners[0].id : var.existing_network
  subnetwork_path     = var.create_network ? "projects/${var.project_id}/regions/${var.region}/subnetworks/${google_compute_subnetwork.runners[0].name}" : var.existing_subnetwork
}

resource "google_service_account" "runner_manager" {
  project      = var.project_id
  account_id   = "${var.name_prefix}-byoc"
  display_name = "open-ci BYOC Runner Manager"
  description  = "Impersonated keylessly by the open-ci control plane to launch zero-IAM ephemeral Spot runners."
}

resource "google_service_account_iam_member" "control_plane_impersonation" {
  service_account_id = google_service_account.runner_manager.name
  role               = "roles/iam.serviceAccountTokenCreator"
  member             = "serviceAccount:${var.hosted_control_plane_sa_email}"
}

resource "google_project_iam_custom_role" "runner_provisioner" {
  project     = var.project_id
  role_id     = local.provisioner_role_id
  title       = "open-ci BYOC Runner Provisioner"
  description = "Least-privilege Compute Engine create/read permissions without iam.serviceAccounts.actAs so runners have zero IAM."

  permissions = [
    "compute.disks.create",
    "compute.disks.get",
    "compute.disks.list",
    "compute.disks.setLabels",
    "compute.disks.use",
    "compute.images.get",
    "compute.images.getFromFamily",
    "compute.images.useReadOnly",
    "compute.instances.create",
    "compute.instances.get",
    "compute.instances.list",
    "compute.instances.setLabels",
    "compute.instances.setMetadata",
    "compute.instances.setTags",
    "compute.networks.list",
    "compute.regions.get",
    "compute.subnetworks.list",
    "compute.subnetworks.use",
    "compute.subnetworks.useExternalIp",
    "compute.zoneOperations.get",
    "compute.zones.get",
  ]
}

resource "google_project_iam_custom_role" "runner_cleaner" {
  project     = var.project_id
  role_id     = local.cleaner_role_id
  title       = "open-ci BYOC Runner Cleaner"
  description = "Scoped delete permissions bound via IAM Condition to open-ci-* ephemeral instances and disks."

  permissions = [
    "compute.disks.delete",
    "compute.instances.delete",
  ]
}

resource "google_project_iam_member" "runner_provisioner_binding" {
  project = var.project_id
  role    = google_project_iam_custom_role.runner_provisioner.name
  member  = "serviceAccount:${google_service_account.runner_manager.email}"
}

resource "google_project_iam_member" "runner_cleaner_binding" {
  project = var.project_id
  role    = google_project_iam_custom_role.runner_cleaner.name
  member  = "serviceAccount:${google_service_account.runner_manager.email}"

  condition {
    title       = "OnlyOpenCiPrefixedResources"
    description = "Restrict VM and disk deletion exclusively to open-ci-* ephemeral runner resources."
    expression  = "resource.name.extract('/instances/{name}').startsWith('open-ci-') || resource.name.extract('/disks/{name}').startsWith('open-ci-')"
  }
}

resource "google_compute_network" "runners" {
  count                   = var.create_network ? 1 : 0
  project                 = var.project_id
  name                    = "${var.name_prefix}-runners-vpc"
  auto_create_subnetworks = false
}

resource "google_compute_subnetwork" "runners" {
  count                    = var.create_network ? 1 : 0
  project                  = var.project_id
  name                     = "${var.name_prefix}-runners-${var.region}"
  region                   = var.region
  network                  = google_compute_network.runners[0].id
  ip_cidr_range            = var.subnet_cidr
  private_ip_google_access = true
}

resource "google_compute_router" "nat" {
  count   = var.create_network ? 1 : 0
  project = var.project_id
  name    = "${var.name_prefix}-nat-router-${var.region}"
  region  = var.region
  network = google_compute_network.runners[0].id
}

resource "google_compute_router_nat" "runners" {
  count                              = var.create_network ? 1 : 0
  project                            = var.project_id
  name                               = "${var.name_prefix}-nat-${var.region}"
  router                             = google_compute_router.nat[0].name
  region                             = var.region
  nat_ip_allocate_option             = "AUTO_ONLY"
  source_subnetwork_ip_ranges_to_nat = "LIST_OF_SUBNETWORKS"

  subnetwork {
    name                    = google_compute_subnetwork.runners[0].id
    source_ip_ranges_to_nat = ["ALL_IP_RANGES"]
  }

  log_config {
    enable = true
    filter = "ERRORS_ONLY"
  }
}

resource "google_compute_firewall" "deny_runner_ingress" {
  project     = var.project_id
  name        = "${var.name_prefix}-deny-runner-ingress"
  network     = local.network_name
  direction   = "INGRESS"
  priority    = 100
  target_tags = [local.runner_network_tag]

  deny {
    protocol = "all"
  }

  source_ranges = ["0.0.0.0/0"]

  lifecycle {
    precondition {
      condition     = var.create_network || (var.existing_network != "" && var.existing_subnetwork != "")
      error_message = "When create_network is false, both existing_network and existing_subnetwork must be specified."
    }
  }
}

resource "google_storage_bucket" "cache" {
  count                       = var.create_cache_bucket ? 1 : 0
  project                     = var.project_id
  name                        = "${var.project_id}-${var.name_prefix}-cache-${var.region}"
  location                    = var.region
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"
  labels                      = local.common_labels

  lifecycle_rule {
    action {
      type = "Delete"
    }
    condition {
      age = var.cache_retention_days
    }
  }
}

resource "google_storage_bucket_iam_member" "cache_access" {
  count  = var.create_cache_bucket ? 1 : 0
  bucket = google_storage_bucket.cache[0].name
  role   = "roles/storage.objectAdmin"
  member = "serviceAccount:${google_service_account.runner_manager.email}"
}
