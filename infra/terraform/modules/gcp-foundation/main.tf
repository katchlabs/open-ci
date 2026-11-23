locals {
  common_labels = merge(
    {
      "open-ci-managed" = "true"
      "open-ci-role"    = "foundation"
    },
    var.labels,
  )
  role_id = replace("${var.name_prefix}_runner_controller", "-", "_")
}

resource "google_compute_network" "runners" {
  project                 = var.project_id
  name                    = "${var.name_prefix}-runners-vpc"
  auto_create_subnetworks = false
}

resource "google_compute_subnetwork" "runners" {
  project                  = var.project_id
  name                     = "${var.name_prefix}-runners-${var.region}"
  region                   = var.region
  network                  = google_compute_network.runners.id
  ip_cidr_range            = var.subnet_cidr
  private_ip_google_access = true
}

resource "google_compute_router" "nat" {
  project = var.project_id
  name    = "${var.name_prefix}-nat-router-${var.region}"
  region  = var.region
  network = google_compute_network.runners.id
}

resource "google_compute_router_nat" "runners" {
  project                            = var.project_id
  name                               = "${var.name_prefix}-nat-${var.region}"
  router                             = google_compute_router.nat.name
  region                             = var.region
  nat_ip_allocate_option             = "AUTO_ONLY"
  source_subnetwork_ip_ranges_to_nat = "LIST_OF_SUBNETWORKS"

  subnetwork {
    name                    = google_compute_subnetwork.runners.id
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
  network     = google_compute_network.runners.id
  direction   = "INGRESS"
  priority    = 65534
  target_tags = ["${var.name_prefix}-runner"]

  deny {
    protocol = "all"
  }

  source_ranges = ["0.0.0.0/0"]
}

resource "google_project_iam_custom_role" "runner_controller" {
  project     = var.project_id
  role_id     = local.role_id
  title       = "open-ci Runner Controller"
  description = "Least-privilege Compute Engine permissions for open-ci ephemeral runner management."

  permissions = [
    "compute.disks.create",
    "compute.disks.delete",
    "compute.disks.get",
    "compute.disks.list",
    "compute.disks.setLabels",
    "compute.disks.use",
    "compute.images.get",
    "compute.images.getFromFamily",
    "compute.images.useReadOnly",
    "compute.instances.create",
    "compute.instances.delete",
    "compute.instances.get",
    "compute.instances.list",
    "compute.instances.setLabels",
    "compute.instances.setMetadata",
    "compute.instances.setTags",
    "compute.subnetworks.use",
    "compute.zoneOperations.get",
    "compute.zones.get",
  ]
}

resource "google_project_iam_member" "controller_compute" {
  count   = var.controller_service_account_email != "" ? 1 : 0
  project = var.project_id
  role    = google_project_iam_custom_role.runner_controller.name
  member  = "serviceAccount:${var.controller_service_account_email}"
}

resource "google_storage_bucket" "backups" {
  project                     = var.project_id
  name                        = "${var.project_id}-${var.name_prefix}-backups"
  location                    = var.region
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"
  labels                      = local.common_labels

  versioning {
    enabled = true
  }

  lifecycle_rule {
    action {
      type = "Delete"
    }
    condition {
      age = var.backup_retention_days
    }
  }
}

resource "google_storage_bucket_iam_member" "controller_backup_writer" {
  count  = var.controller_service_account_email != "" ? 1 : 0
  bucket = google_storage_bucket.backups.name
  role   = "roles/storage.objectCreator"
  member = "serviceAccount:${var.controller_service_account_email}"
}
