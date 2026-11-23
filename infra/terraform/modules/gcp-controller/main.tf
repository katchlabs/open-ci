locals {
  common_labels = merge(
    {
      "open-ci-managed" = "true"
      "open-ci-role"    = "controller"
    },
    var.labels,
  )
  secret_names = toset([
    "webhook-secret",
    "bootstrap-hmac",
    "operator-token",
    "github-app-private-key",
  ])
}

resource "google_service_account" "controller" {
  project      = var.project_id
  account_id   = "${var.name_prefix}-controller"
  display_name = "open-ci Control Plane"
}

resource "google_compute_address" "controller" {
  project      = var.project_id
  name         = "${var.name_prefix}-controller-ip"
  region       = var.region
  address_type = "EXTERNAL"
}

resource "google_compute_firewall" "controller_https" {
  project     = var.project_id
  name        = "${var.name_prefix}-controller-https"
  network     = var.network_self_link
  direction   = "INGRESS"
  target_tags = ["${var.name_prefix}-controller"]

  allow {
    protocol = "tcp"
    ports    = ["80", "443"]
  }

  source_ranges = ["0.0.0.0/0"]
}

resource "google_secret_manager_secret" "controller" {
  for_each  = local.secret_names
  project   = var.project_id
  secret_id = "${var.name_prefix}-${each.value}"
  labels    = local.common_labels

  replication {
    auto {}
  }
}

resource "google_secret_manager_secret_iam_member" "controller_accessor" {
  for_each  = google_secret_manager_secret.controller
  project   = var.project_id
  secret_id = each.value.secret_id
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.controller.email}"
}

resource "google_compute_instance" "controller" {
  project      = var.project_id
  zone         = var.zone
  name         = "${var.name_prefix}-controller"
  machine_type = var.machine_type
  tags         = ["${var.name_prefix}-controller"]
  labels       = local.common_labels

  boot_disk {
    initialize_params {
      image = var.boot_image
      size  = 25
      type  = "pd-balanced"
    }
  }

  network_interface {
    subnetwork = var.subnet_self_link
    access_config {
      nat_ip = google_compute_address.controller.address
    }
  }

  service_account {
    email  = google_service_account.controller.email
    scopes = ["https://www.googleapis.com/auth/cloud-platform"]
  }

  shielded_instance_config {
    enable_secure_boot          = true
    enable_vtpm                 = true
    enable_integrity_monitoring = true
  }

  metadata = {
    enable-oslogin = "TRUE"
    open-ci-domain = var.domain
  }
}
