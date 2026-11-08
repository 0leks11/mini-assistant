#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Location {
    pub id: String,
    pub name: String,
    pub country: String,
    pub latitude: f64,
    pub longitude: f64,
    pub timezone: String,
    pub aliases: Vec<String>,
}

pub fn default_locations() -> Vec<Location> {
    vec![
        Location {
            id: "tbilisi".to_string(),
            name: "Тбилиси".to_string(),
            country: "GE".to_string(),
            latitude: 41.7151,
            longitude: 44.8271,
            timezone: "Asia/Tbilisi".to_string(),
            aliases: vec![
                "тбилиси".to_string(),
                "тифлис".to_string(),
                "tbilisi".to_string(),
            ],
        },
        Location {
            id: "batumi".to_string(),
            name: "Батуми".to_string(),
            country: "GE".to_string(),
            latitude: 41.6434,
            longitude: 41.6399,
            timezone: "Asia/Tbilisi".to_string(),
            aliases: vec!["батуми".to_string(), "batumi".to_string()],
        },
        Location {
            id: "tsikhisdziri".to_string(),
            name: "Цихисдзири".to_string(),
            country: "GE".to_string(),
            latitude: 41.7630,
            longitude: 41.7540,
            timezone: "Asia/Tbilisi".to_string(),
            aliases: vec![
                "цихисдзири".to_string(),
                "sihidzyri".to_string(),
                "tsikhisdziri".to_string(),
            ],
        },
    ]
}

#[derive(Debug, Clone)]
pub struct LocationRegistry {
    locations: Vec<Location>,
}

impl Default for LocationRegistry {
    fn default() -> Self {
        Self {
            locations: default_locations(),
        }
    }
}

impl LocationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, location: Location) {
        if let Some(pos) = self.locations.iter().position(|l| l.id == location.id) {
            self.locations[pos] = location;
        } else {
            self.locations.push(location);
        }
    }

    /// Resolve a location by exact id, alias or case-insensitive name match
    pub fn resolve(&self, query: &str) -> Option<&Location> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return None;
        }

        // 1. Direct ID match
        if let Some(loc) = self.locations.iter().find(|l| l.id == q) {
            return Some(loc);
        }

        // 2. Exact alias match
        if let Some(loc) = self
            .locations
            .iter()
            .find(|l| l.aliases.iter().any(|a| a == &q))
        {
            return Some(loc);
        }

        // 3. Substring match in name or aliases
        self.locations.iter().find(|l| {
            l.name.to_lowercase().contains(&q) || l.aliases.iter().any(|a| a.contains(&q))
        })
    }

    pub fn list(&self) -> &[Location] {
        &self.locations
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_exact_id() {
        let reg = LocationRegistry::new();
        assert_eq!(reg.resolve("batumi").unwrap().id, "batumi");
        assert_eq!(reg.resolve("tbilisi").unwrap().id, "tbilisi");
        assert_eq!(reg.resolve("tsikhisdziri").unwrap().id, "tsikhisdziri");
    }

    #[test]
    fn test_resolve_aliases() {
        let reg = LocationRegistry::new();
        assert_eq!(reg.resolve("Батуми").unwrap().id, "batumi");
        assert_eq!(reg.resolve("Тбилиси").unwrap().id, "tbilisi");
        assert_eq!(reg.resolve("Тифлис").unwrap().id, "tbilisi");
        assert_eq!(reg.resolve("sihidzyri").unwrap().id, "tsikhisdziri");
        assert_eq!(reg.resolve("Цихисдзири").unwrap().id, "tsikhisdziri");
    }

    #[test]
    fn test_resolve_unknown() {
        let reg = LocationRegistry::new();
        assert!(reg.resolve("london").is_none());
        assert!(reg.resolve("").is_none());
    }

    #[test]
    fn test_register_dynamic_location() {
        let mut reg = LocationRegistry::new();
        let moscow = Location {
            id: "moscow".to_string(),
            name: "Москва".to_string(),
            country: "RU".to_string(),
            latitude: 55.75,
            longitude: 37.61,
            timezone: "Europe/Moscow".to_string(),
            aliases: vec!["москва".to_string(), "moscow".to_string()],
        };
        reg.register(moscow);
        assert_eq!(reg.resolve("Москва").unwrap().id, "moscow");
        assert_eq!(reg.resolve("moscow").unwrap().id, "moscow");
    }
}
