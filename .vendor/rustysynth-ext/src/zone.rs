use crate::error::SoundFontError;
use crate::generator::Generator;
use crate::zone_info::ZoneInfo;

#[non_exhaustive]
pub(crate) struct Zone {
    pub(crate) generators: Vec<Generator>,
}

impl Zone {
    pub(crate) fn empty() -> Self {
        Self {
            generators: Vec::new(),
        }
    }

    fn new(info: &ZoneInfo, generators: &[Generator]) -> Result<Self, SoundFontError> {
        // Copy the zone's generator span exactly once, with a single allocation.
        // The span indexes come straight from the file (pbag/ibag records), so
        // they must be validated before slicing: a negative or overflowed span
        // would otherwise panic on malformed input.
        let start = info.generator_index as usize;
        let count = info.generator_count as usize;

        let Some(end) = start.checked_add(count) else {
            return Err(SoundFontError::InvalidZoneList);
        };
        if end > generators.len() {
            return Err(SoundFontError::InvalidZoneList);
        }

        let mut segment: Vec<Generator> = Vec::with_capacity(count);
        segment.extend_from_slice(&generators[start..end]);

        Ok(Self {
            generators: segment,
        })
    }

    pub(crate) fn create(
        infos: &[ZoneInfo],
        generators: &[Generator],
    ) -> Result<Vec<Zone>, SoundFontError> {
        if infos.len() <= 1 {
            return Err(SoundFontError::ZoneNotFound);
        }

        // The last one is the terminator.
        let count = infos.len() - 1;

        let mut zones: Vec<Zone> = Vec::with_capacity(count);
        for info in infos.iter().take(count) {
            zones.push(Zone::new(info, generators)?);
        }

        Ok(zones)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(generator_index: i32, generator_count: i32) -> ZoneInfo {
        ZoneInfo {
            generator_index,
            generator_count,
            modulator_index: 0,
            modulator_count: 0,
        }
    }

    fn generators(len: usize) -> Vec<Generator> {
        (0..len)
            .map(|i| Generator {
                generator_type: i as u16,
                value: 0,
            })
            .collect()
    }

    #[test]
    fn rejects_negative_generator_span() {
        let span = info(0, -1);
        assert!(matches!(
            Zone::new(&span, &generators(1)),
            Err(SoundFontError::InvalidZoneList)
        ));
    }

    #[test]
    fn rejects_generator_span_past_the_end() {
        let span = info(1, 2);
        assert!(matches!(
            Zone::new(&span, &generators(2)),
            Err(SoundFontError::InvalidZoneList)
        ));
    }

    #[test]
    fn accepts_a_valid_generator_span() {
        let span = info(0, 2);
        let zone = Zone::new(&span, &generators(3)).unwrap();
        assert_eq!(zone.generators.len(), 2);
        assert_eq!(zone.generators[0].generator_type, 0);
        assert_eq!(zone.generators[1].generator_type, 1);
    }
}
