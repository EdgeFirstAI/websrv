// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! EdgeFirst Studio API integration handlers.
//!
//! Provides endpoints for:
//! - Listing projects from EdgeFirst Studio
//! - Getting auto-labeling labels (COCO 80 classes)

use actix_web::{web, HttpResponse, Responder};
use log::error;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::upload::{UploadErrorResponse, UploadManager};

/// Response for project list
#[derive(Serialize, Deserialize)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
}

/// Response for label list
#[derive(Serialize, Deserialize)]
pub struct LabelInfo {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[serde(default)]
    pub default: bool,
}

/// Trait for accessing upload manager from server context
pub trait StudioContext {
    fn upload_manager(&self) -> &Arc<UploadManager>;
}

/// GET /api/studio/projects - List projects from EdgeFirst Studio
pub async fn list_studio_projects<T: StudioContext>(data: web::Data<T>) -> impl Responder {
    let client_lock = data.upload_manager().client.read().await;

    match &*client_lock {
        Some(client) => match client.projects(None).await {
            Ok(projects) => {
                let project_infos: Vec<ProjectInfo> = projects
                    .into_iter()
                    .map(|p| ProjectInfo {
                        id: p.id().value().to_string(),
                        name: p.name().to_string(),
                    })
                    .collect();
                HttpResponse::Ok().json(project_infos)
            }
            Err(e) => {
                error!("Failed to fetch projects: {}", e);
                HttpResponse::InternalServerError().json(UploadErrorResponse {
                    error: format!("Failed to fetch projects: {}", e),
                })
            }
        },
        None => HttpResponse::Unauthorized().json(UploadErrorResponse {
            error: "Not authenticated with EdgeFirst Studio".to_string(),
        }),
    }
}

/// GET /api/studio/projects/{id}/labels - Get labels for auto-labeling
/// Returns the 80 COCO dataset labels used by YOLOX for AGTG autobox prompts.
/// These labels drive the SAM2 model for automatic annotation.
/// In the future this will be configurable; for now we return the standard COCO
/// classes.
pub async fn list_project_labels<T: StudioContext>(
    _path: web::Path<String>,
    data: web::Data<T>,
) -> impl Responder {
    let client_lock = data.upload_manager().client.read().await;

    match &*client_lock {
        Some(_client) => {
            let coco_labels = get_coco_labels();
            HttpResponse::Ok().json(coco_labels)
        }
        None => HttpResponse::Unauthorized().json(UploadErrorResponse {
            error: "Not authenticated with EdgeFirst Studio".to_string(),
        }),
    }
}

/// Get COCO 80 classes used by YOLOX for AGTG autobox prompts.
/// "person" is selected by default as the most common use case.
pub fn get_coco_labels() -> Vec<LabelInfo> {
    vec![
        // Person (default selected)
        LabelInfo {
            id: "person".to_string(),
            name: "Person".to_string(),
            default: true,
        },
        // Vehicles
        LabelInfo {
            id: "bicycle".to_string(),
            name: "Bicycle".to_string(),
            default: false,
        },
        LabelInfo {
            id: "car".to_string(),
            name: "Car".to_string(),
            default: false,
        },
        LabelInfo {
            id: "motorcycle".to_string(),
            name: "Motorcycle".to_string(),
            default: false,
        },
        LabelInfo {
            id: "airplane".to_string(),
            name: "Airplane".to_string(),
            default: false,
        },
        LabelInfo {
            id: "bus".to_string(),
            name: "Bus".to_string(),
            default: false,
        },
        LabelInfo {
            id: "train".to_string(),
            name: "Train".to_string(),
            default: false,
        },
        LabelInfo {
            id: "truck".to_string(),
            name: "Truck".to_string(),
            default: false,
        },
        LabelInfo {
            id: "boat".to_string(),
            name: "Boat".to_string(),
            default: false,
        },
        // Outdoor objects
        LabelInfo {
            id: "traffic_light".to_string(),
            name: "Traffic Light".to_string(),
            default: false,
        },
        LabelInfo {
            id: "fire_hydrant".to_string(),
            name: "Fire Hydrant".to_string(),
            default: false,
        },
        LabelInfo {
            id: "stop_sign".to_string(),
            name: "Stop Sign".to_string(),
            default: false,
        },
        LabelInfo {
            id: "parking_meter".to_string(),
            name: "Parking Meter".to_string(),
            default: false,
        },
        LabelInfo {
            id: "bench".to_string(),
            name: "Bench".to_string(),
            default: false,
        },
        // Animals
        LabelInfo {
            id: "bird".to_string(),
            name: "Bird".to_string(),
            default: false,
        },
        LabelInfo {
            id: "cat".to_string(),
            name: "Cat".to_string(),
            default: false,
        },
        LabelInfo {
            id: "dog".to_string(),
            name: "Dog".to_string(),
            default: false,
        },
        LabelInfo {
            id: "horse".to_string(),
            name: "Horse".to_string(),
            default: false,
        },
        LabelInfo {
            id: "sheep".to_string(),
            name: "Sheep".to_string(),
            default: false,
        },
        LabelInfo {
            id: "cow".to_string(),
            name: "Cow".to_string(),
            default: false,
        },
        LabelInfo {
            id: "elephant".to_string(),
            name: "Elephant".to_string(),
            default: false,
        },
        LabelInfo {
            id: "bear".to_string(),
            name: "Bear".to_string(),
            default: false,
        },
        LabelInfo {
            id: "zebra".to_string(),
            name: "Zebra".to_string(),
            default: false,
        },
        LabelInfo {
            id: "giraffe".to_string(),
            name: "Giraffe".to_string(),
            default: false,
        },
        // Accessories
        LabelInfo {
            id: "backpack".to_string(),
            name: "Backpack".to_string(),
            default: false,
        },
        LabelInfo {
            id: "umbrella".to_string(),
            name: "Umbrella".to_string(),
            default: false,
        },
        LabelInfo {
            id: "handbag".to_string(),
            name: "Handbag".to_string(),
            default: false,
        },
        LabelInfo {
            id: "tie".to_string(),
            name: "Tie".to_string(),
            default: false,
        },
        LabelInfo {
            id: "suitcase".to_string(),
            name: "Suitcase".to_string(),
            default: false,
        },
        // Sports
        LabelInfo {
            id: "frisbee".to_string(),
            name: "Frisbee".to_string(),
            default: false,
        },
        LabelInfo {
            id: "skis".to_string(),
            name: "Skis".to_string(),
            default: false,
        },
        LabelInfo {
            id: "snowboard".to_string(),
            name: "Snowboard".to_string(),
            default: false,
        },
        LabelInfo {
            id: "sports_ball".to_string(),
            name: "Sports Ball".to_string(),
            default: false,
        },
        LabelInfo {
            id: "kite".to_string(),
            name: "Kite".to_string(),
            default: false,
        },
        LabelInfo {
            id: "baseball_bat".to_string(),
            name: "Baseball Bat".to_string(),
            default: false,
        },
        LabelInfo {
            id: "baseball_glove".to_string(),
            name: "Baseball Glove".to_string(),
            default: false,
        },
        LabelInfo {
            id: "skateboard".to_string(),
            name: "Skateboard".to_string(),
            default: false,
        },
        LabelInfo {
            id: "surfboard".to_string(),
            name: "Surfboard".to_string(),
            default: false,
        },
        LabelInfo {
            id: "tennis_racket".to_string(),
            name: "Tennis Racket".to_string(),
            default: false,
        },
        // Kitchen
        LabelInfo {
            id: "bottle".to_string(),
            name: "Bottle".to_string(),
            default: false,
        },
        LabelInfo {
            id: "wine_glass".to_string(),
            name: "Wine Glass".to_string(),
            default: false,
        },
        LabelInfo {
            id: "cup".to_string(),
            name: "Cup".to_string(),
            default: false,
        },
        LabelInfo {
            id: "fork".to_string(),
            name: "Fork".to_string(),
            default: false,
        },
        LabelInfo {
            id: "knife".to_string(),
            name: "Knife".to_string(),
            default: false,
        },
        LabelInfo {
            id: "spoon".to_string(),
            name: "Spoon".to_string(),
            default: false,
        },
        LabelInfo {
            id: "bowl".to_string(),
            name: "Bowl".to_string(),
            default: false,
        },
        // Food
        LabelInfo {
            id: "banana".to_string(),
            name: "Banana".to_string(),
            default: false,
        },
        LabelInfo {
            id: "apple".to_string(),
            name: "Apple".to_string(),
            default: false,
        },
        LabelInfo {
            id: "sandwich".to_string(),
            name: "Sandwich".to_string(),
            default: false,
        },
        LabelInfo {
            id: "orange".to_string(),
            name: "Orange".to_string(),
            default: false,
        },
        LabelInfo {
            id: "broccoli".to_string(),
            name: "Broccoli".to_string(),
            default: false,
        },
        LabelInfo {
            id: "carrot".to_string(),
            name: "Carrot".to_string(),
            default: false,
        },
        LabelInfo {
            id: "hot_dog".to_string(),
            name: "Hot Dog".to_string(),
            default: false,
        },
        LabelInfo {
            id: "pizza".to_string(),
            name: "Pizza".to_string(),
            default: false,
        },
        LabelInfo {
            id: "donut".to_string(),
            name: "Donut".to_string(),
            default: false,
        },
        LabelInfo {
            id: "cake".to_string(),
            name: "Cake".to_string(),
            default: false,
        },
        // Furniture
        LabelInfo {
            id: "chair".to_string(),
            name: "Chair".to_string(),
            default: false,
        },
        LabelInfo {
            id: "couch".to_string(),
            name: "Couch".to_string(),
            default: false,
        },
        LabelInfo {
            id: "potted_plant".to_string(),
            name: "Potted Plant".to_string(),
            default: false,
        },
        LabelInfo {
            id: "bed".to_string(),
            name: "Bed".to_string(),
            default: false,
        },
        LabelInfo {
            id: "dining_table".to_string(),
            name: "Dining Table".to_string(),
            default: false,
        },
        LabelInfo {
            id: "toilet".to_string(),
            name: "Toilet".to_string(),
            default: false,
        },
        // Electronics
        LabelInfo {
            id: "tv".to_string(),
            name: "TV".to_string(),
            default: false,
        },
        LabelInfo {
            id: "laptop".to_string(),
            name: "Laptop".to_string(),
            default: false,
        },
        LabelInfo {
            id: "mouse".to_string(),
            name: "Mouse".to_string(),
            default: false,
        },
        LabelInfo {
            id: "remote".to_string(),
            name: "Remote".to_string(),
            default: false,
        },
        LabelInfo {
            id: "keyboard".to_string(),
            name: "Keyboard".to_string(),
            default: false,
        },
        LabelInfo {
            id: "cell_phone".to_string(),
            name: "Cell Phone".to_string(),
            default: false,
        },
        // Appliances
        LabelInfo {
            id: "microwave".to_string(),
            name: "Microwave".to_string(),
            default: false,
        },
        LabelInfo {
            id: "oven".to_string(),
            name: "Oven".to_string(),
            default: false,
        },
        LabelInfo {
            id: "toaster".to_string(),
            name: "Toaster".to_string(),
            default: false,
        },
        LabelInfo {
            id: "sink".to_string(),
            name: "Sink".to_string(),
            default: false,
        },
        LabelInfo {
            id: "refrigerator".to_string(),
            name: "Refrigerator".to_string(),
            default: false,
        },
        // Indoor objects
        LabelInfo {
            id: "book".to_string(),
            name: "Book".to_string(),
            default: false,
        },
        LabelInfo {
            id: "clock".to_string(),
            name: "Clock".to_string(),
            default: false,
        },
        LabelInfo {
            id: "vase".to_string(),
            name: "Vase".to_string(),
            default: false,
        },
        LabelInfo {
            id: "scissors".to_string(),
            name: "Scissors".to_string(),
            default: false,
        },
        LabelInfo {
            id: "teddy_bear".to_string(),
            name: "Teddy Bear".to_string(),
            default: false,
        },
        LabelInfo {
            id: "hair_drier".to_string(),
            name: "Hair Drier".to_string(),
            default: false,
        },
        LabelInfo {
            id: "toothbrush".to_string(),
            name: "Toothbrush".to_string(),
            default: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coco_labels_expected_count() {
        let coco_labels = get_coco_labels();
        assert_eq!(coco_labels.len(), 80, "COCO dataset should have 80 classes");
    }

    #[test]
    fn test_coco_labels_person_is_default() {
        let coco_labels = get_coco_labels();
        let person_label = coco_labels
            .iter()
            .find(|l| l.id == "person")
            .expect("Person label should exist");

        assert!(person_label.default, "Person should be the default label");

        // Verify only one default
        let default_count = coco_labels.iter().filter(|l| l.default).count();
        assert_eq!(default_count, 1, "Only person should be default");
    }

    #[test]
    fn test_coco_labels_common_classes() {
        let coco_labels = get_coco_labels();

        // Common automotive/surveillance classes that users expect
        let expected_classes = vec![
            "person",
            "bicycle",
            "car",
            "motorcycle",
            "bus",
            "truck",
            "traffic_light",
            "stop_sign",
            "dog",
            "cat",
        ];

        for expected in expected_classes {
            assert!(
                coco_labels.iter().any(|l| l.id == expected),
                "COCO labels should include '{}'",
                expected
            );
        }
    }

    #[test]
    fn test_project_info_structure() {
        let project = ProjectInfo {
            id: "12345".to_string(),
            name: "Test Project".to_string(),
        };

        let json = serde_json::to_string(&project).expect("Failed to serialize");
        assert!(json.contains("\"id\":\"12345\""));
        assert!(json.contains("\"name\":\"Test Project\""));
    }

    #[test]
    fn test_label_info_structure() {
        // Default label
        let label = LabelInfo {
            id: "person".to_string(),
            name: "Person".to_string(),
            default: true,
        };

        let json = serde_json::to_string(&label).expect("Failed to serialize");
        assert!(json.contains("\"id\":\"person\""));
        assert!(json.contains("\"name\":\"Person\""));
        assert!(json.contains("\"default\":true"));

        // Non-default label should not include "default" field
        let label = LabelInfo {
            id: "car".to_string(),
            name: "Car".to_string(),
            default: false,
        };

        let json = serde_json::to_string(&label).expect("Failed to serialize");
        assert!(json.contains("\"id\":\"car\""));
        assert!(!json.contains("default"));
    }
}
