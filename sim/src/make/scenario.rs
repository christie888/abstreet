use crate::{
    CarID, DrivingGoal, ParkingSpot, PersonID, SidewalkPOI, SidewalkSpot, Sim, TripEndpoint,
    TripSpec, Vehicle, VehicleSpec, VehicleType, BIKE_LENGTH, MAX_CAR_LENGTH, MIN_CAR_LENGTH,
};
use abstutil::{prettyprint_usize, Counter, Timer};
use geom::{Distance, Duration, Speed, Time};
use map_model::{
    BuildingID, BusRouteID, BusStopID, IntersectionID, Map, PathConstraints, Position, RoadID,
};
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use rand_xorshift::XorShiftRng;
use serde_derive::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

// How to start a simulation.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Scenario {
    pub scenario_name: String,
    pub map_name: String,

    pub people: Vec<PersonSpec>,
    // None means seed all buses. Otherwise the route name must be present here.
    pub only_seed_buses: Option<BTreeSet<String>>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct PersonSpec {
    pub id: PersonID,
    // Just used for debugging
    pub orig_id: (usize, usize),
    pub trips: Vec<IndividTrip>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct IndividTrip {
    pub depart: Time,
    pub trip: SpawnTrip,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub enum SpawnTrip {
    // Only for interactive / debug trips
    VehicleAppearing {
        start: Position,
        goal: DrivingGoal,
        is_bike: bool,
    },
    FromBorder {
        i: IntersectionID,
        goal: DrivingGoal,
        // For bikes starting at a border, use FromBorder. UsingBike implies a walk->bike trip.
        is_bike: bool,
    },
    UsingParkedCar(BuildingID, DrivingGoal),
    UsingBike(SidewalkSpot, DrivingGoal),
    JustWalking(SidewalkSpot, SidewalkSpot),
    UsingTransit(SidewalkSpot, SidewalkSpot, BusRouteID, BusStopID, BusStopID),
}

impl Scenario {
    // Any case where map edits could change the calls to the RNG, we have to fork.
    pub fn instantiate(&self, sim: &mut Sim, map: &Map, rng: &mut XorShiftRng, timer: &mut Timer) {
        sim.set_name(self.scenario_name.clone());

        timer.start(format!("Instantiating {}", self.scenario_name));

        if let Some(ref routes) = self.only_seed_buses {
            for route in map.get_all_bus_routes() {
                if routes.contains(&route.name) {
                    sim.seed_bus_route(route, map, timer);
                }
            }
        } else {
            // All of them
            for route in map.get_all_bus_routes() {
                sim.seed_bus_route(route, map, timer);
            }
        }

        timer.start_iter("trips for People", self.people.len());
        let mut spawner = sim.make_spawner();
        let mut parked_cars: Vec<(Vehicle, BuildingID)> = Vec::new();
        for p in &self.people {
            timer.next();

            let (vehicle_specs, cars_initially_parked_at, vehicle_foreach_trip) =
                p.get_vehicles(rng);
            sim.new_person(p.id, Scenario::rand_ped_speed(rng), vehicle_specs);
            let person = sim.get_person(p.id);
            for (idx, b) in cars_initially_parked_at {
                parked_cars.push((person.vehicles[idx].clone(), b));
            }
            for (t, maybe_idx) in p.trips.iter().zip(vehicle_foreach_trip) {
                // The RNG call might change over edits for picking the spawning lane from a border
                // with multiple choices for a vehicle type.
                let mut tmp_rng = abstutil::fork_rng(rng);
                if let Some(spec) = t.trip.clone().to_trip_spec(
                    maybe_idx.map(|idx| person.vehicles[idx].id),
                    &mut tmp_rng,
                    map,
                ) {
                    spawner.schedule_trip(person, t.depart, spec, map);
                } else {
                    timer.warn(format!("Couldn't turn {:?} into a trip", t.trip));
                }
            }
        }

        // parked_cars is stable over map edits, so don't fork.
        parked_cars.shuffle(rng);
        seed_parked_cars(parked_cars, sim, map, rng, timer);

        sim.flush_spawner(spawner, map, timer);
        timer.stop(format!("Instantiating {}", self.scenario_name));
    }

    pub fn save(&self) {
        abstutil::write_binary(
            abstutil::path_scenario(&self.map_name, &self.scenario_name),
            self,
        );
    }

    pub fn empty(map: &Map, name: &str) -> Scenario {
        Scenario {
            scenario_name: name.to_string(),
            map_name: map.get_name().to_string(),
            people: Vec::new(),
            only_seed_buses: Some(BTreeSet::new()),
        }
    }

    pub fn rand_car(rng: &mut XorShiftRng) -> VehicleSpec {
        let length = Scenario::rand_dist(rng, MIN_CAR_LENGTH, MAX_CAR_LENGTH);
        VehicleSpec {
            vehicle_type: VehicleType::Car,
            length,
            max_speed: None,
        }
    }

    pub fn rand_bike(rng: &mut XorShiftRng) -> VehicleSpec {
        let max_speed = Some(Scenario::rand_speed(
            rng,
            Speed::miles_per_hour(8.0),
            Speed::miles_per_hour(10.0),
        ));
        VehicleSpec {
            vehicle_type: VehicleType::Bike,
            length: BIKE_LENGTH,
            max_speed,
        }
    }

    pub fn rand_dist(rng: &mut XorShiftRng, low: Distance, high: Distance) -> Distance {
        assert!(high > low);
        Distance::meters(rng.gen_range(low.inner_meters(), high.inner_meters()))
    }

    fn rand_speed(rng: &mut XorShiftRng, low: Speed, high: Speed) -> Speed {
        assert!(high > low);
        Speed::meters_per_second(rng.gen_range(
            low.inner_meters_per_second(),
            high.inner_meters_per_second(),
        ))
    }

    pub fn rand_ped_speed(rng: &mut XorShiftRng) -> Speed {
        Scenario::rand_speed(rng, Speed::miles_per_hour(2.0), Speed::miles_per_hour(3.0))
    }

    // TODO Utter hack. Blindly repeats all trips taken by each person every day. If
    // avoid_inbound_trips is true, then don't repeat driving trips that start outside the map and
    // come in, because those often lead to parking spots leaking. This isn't realistic, but none
    // of this is; even the original 1-day scenario doesn't yet guarantee continuity of people. A
    // person might be in the middle of one trip, and they start the next one!
    pub fn repeat_days(mut self, days: usize, avoid_inbound_trips: bool) -> Scenario {
        self.scenario_name = format!("{} repeated for {} days", self.scenario_name, days);
        for person in &mut self.people {
            let mut trips = Vec::new();
            let mut offset = Duration::ZERO;
            for day in 0..days {
                for trip in &person.trips {
                    let inbound = match trip.trip {
                        SpawnTrip::VehicleAppearing { is_bike, .. } => !is_bike,
                        _ => false,
                    };
                    if day > 0 && inbound && avoid_inbound_trips {
                        continue;
                    }

                    trips.push(IndividTrip {
                        depart: trip.depart + offset,
                        trip: trip.trip.clone(),
                    });
                }
                offset += Duration::hours(24);
            }
            person.trips = trips;
        }
        self
    }

    pub fn count_parked_cars_per_bldg(&self) -> Counter<BuildingID> {
        let mut per_bldg = Counter::new();
        // Pass in a dummy RNG
        let mut rng = XorShiftRng::from_seed([0; 16]);
        for p in &self.people {
            let (_, cars_initially_parked_at, _) = p.get_vehicles(&mut rng);
            for (_, b) in cars_initially_parked_at {
                per_bldg.inc(b);
            }
        }
        per_bldg
    }

    pub fn remove_weird_schedules(mut self, map: &Map) -> Scenario {
        let orig = self.people.len();
        self.people.retain(|person| {
            // Verify that the trip start/endpoints of each person match up
            let mut ok = true;
            for pair in person.trips.iter().zip(person.trips.iter().skip(1)) {
                // Once off-map, re-enter via any border node.
                let end_bldg = match pair.0.trip.end() {
                    TripEndpoint::Bldg(b) => Some(b),
                    TripEndpoint::Border(_) => None,
                };
                let start_bldg = match pair.1.trip.start(map) {
                    TripEndpoint::Bldg(b) => Some(b),
                    TripEndpoint::Border(_) => None,
                };
                if end_bldg != start_bldg {
                    ok = false;
                    println!(
                        "{:?} warps between some trips, from {:?} to {:?}",
                        person.orig_id, end_bldg, start_bldg
                    );
                    break;
                }
            }
            ok
        });
        println!(
            "{} of {} people have nonsense schedules",
            prettyprint_usize(orig - self.people.len()),
            prettyprint_usize(orig)
        );
        // Fix up IDs
        for (idx, person) in self.people.iter_mut().enumerate() {
            person.id = PersonID(idx);
        }
        self
    }
}

fn seed_parked_cars(
    parked_cars: Vec<(Vehicle, BuildingID)>,
    sim: &mut Sim,
    map: &Map,
    base_rng: &mut XorShiftRng,
    timer: &mut Timer,
) {
    let mut open_spots_per_road: BTreeMap<RoadID, Vec<ParkingSpot>> = BTreeMap::new();
    for spot in sim.get_all_parking_spots().1 {
        let r = match spot {
            ParkingSpot::Onstreet(l, _) => map.get_l(l).parent,
            ParkingSpot::Offstreet(b, _) => map.get_l(map.get_b(b).sidewalk()).parent,
        };
        open_spots_per_road
            .entry(r)
            .or_insert_with(Vec::new)
            .push(spot);
    }
    // Changing parking on one road shouldn't affect far-off roads. Fork carefully.
    for r in map.all_roads() {
        let mut tmp_rng = abstutil::fork_rng(base_rng);
        if let Some(ref mut spots) = open_spots_per_road.get_mut(&r.id) {
            spots.shuffle(&mut tmp_rng);
        }
    }

    timer.start_iter("seed parked cars", parked_cars.len());
    let mut ok = true;
    for (vehicle, b) in parked_cars {
        timer.next();
        if !ok {
            continue;
        }
        if let Some(spot) = find_spot_near_building(b, &mut open_spots_per_road, map, timer) {
            sim.seed_parked_car(vehicle, spot);
        } else {
            timer.warn("Not enough room to seed parked cars.".to_string());
            ok = false;
        }
    }
}

// Pick a parking spot for this building. If the building's road has a free spot, use it. If not,
// start BFSing out from the road in a deterministic way until finding a nearby road with an open
// spot.
fn find_spot_near_building(
    b: BuildingID,
    open_spots_per_road: &mut BTreeMap<RoadID, Vec<ParkingSpot>>,
    map: &Map,
    timer: &mut Timer,
) -> Option<ParkingSpot> {
    let mut roads_queue: VecDeque<RoadID> = VecDeque::new();
    let mut visited: HashSet<RoadID> = HashSet::new();
    {
        let start = map.building_to_road(b).id;
        roads_queue.push_back(start);
        visited.insert(start);
    }

    loop {
        if roads_queue.is_empty() {
            timer.warn(format!(
                "Giving up looking for a free parking spot, searched {} roads of {}: {:?}",
                visited.len(),
                open_spots_per_road.len(),
                visited
            ));
        }
        let r = roads_queue.pop_front()?;
        if let Some(spots) = open_spots_per_road.get_mut(&r) {
            // TODO With some probability, skip this available spot and park farther away
            if !spots.is_empty() {
                return spots.pop();
            }
        }

        for next_r in map.get_next_roads(r).into_iter() {
            if !visited.contains(&next_r) {
                roads_queue.push_back(next_r);
                visited.insert(next_r);
            }
        }
    }
}

impl SpawnTrip {
    fn to_trip_spec(
        self,
        use_vehicle: Option<CarID>,
        rng: &mut XorShiftRng,
        map: &Map,
    ) -> Option<TripSpec> {
        match self {
            SpawnTrip::VehicleAppearing { start, goal, .. } => Some(TripSpec::VehicleAppearing {
                start_pos: start,
                goal,
                use_vehicle: use_vehicle.unwrap(),
                retry_if_no_room: true,
            }),
            SpawnTrip::FromBorder {
                i, goal, is_bike, ..
            } => Some(TripSpec::VehicleAppearing {
                start_pos: {
                    let l = *map
                        .get_i(i)
                        .get_outgoing_lanes(
                            map,
                            if is_bike {
                                PathConstraints::Bike
                            } else {
                                PathConstraints::Car
                            },
                        )
                        .choose(rng)?;
                    // TODO We could be more precise and say exactly what vehicle will be used here
                    TripSpec::spawn_vehicle_at(Position::new(l, Distance::ZERO), is_bike, map)?
                },
                goal,
                use_vehicle: use_vehicle.unwrap(),
                retry_if_no_room: true,
            }),
            SpawnTrip::UsingParkedCar(start_bldg, goal) => Some(TripSpec::UsingParkedCar {
                start_bldg,
                goal,
                car: use_vehicle.unwrap(),
            }),
            SpawnTrip::UsingBike(start, goal) => Some(TripSpec::UsingBike {
                bike: use_vehicle.unwrap(),
                start,
                goal,
            }),
            SpawnTrip::JustWalking(start, goal) => Some(TripSpec::JustWalking { start, goal }),
            SpawnTrip::UsingTransit(start, goal, route, stop1, stop2) => {
                Some(TripSpec::UsingTransit {
                    start,
                    goal,
                    route,
                    stop1,
                    stop2,
                })
            }
        }
    }

    pub fn start(&self, map: &Map) -> TripEndpoint {
        match self {
            SpawnTrip::VehicleAppearing { ref start, .. } => {
                TripEndpoint::Border(map.get_l(start.lane()).src_i)
            }
            SpawnTrip::FromBorder { i, .. } => TripEndpoint::Border(*i),
            SpawnTrip::UsingParkedCar(b, _) => TripEndpoint::Bldg(*b),
            SpawnTrip::UsingBike(ref spot, _)
            | SpawnTrip::JustWalking(ref spot, _)
            | SpawnTrip::UsingTransit(ref spot, _, _, _, _) => match spot.connection {
                SidewalkPOI::Building(b) => TripEndpoint::Bldg(b),
                SidewalkPOI::Border(i) => TripEndpoint::Border(i),
                _ => unreachable!(),
            },
        }
    }

    pub fn end(&self) -> TripEndpoint {
        match self {
            SpawnTrip::VehicleAppearing { ref goal, .. }
            | SpawnTrip::FromBorder { ref goal, .. }
            | SpawnTrip::UsingParkedCar(_, ref goal)
            | SpawnTrip::UsingBike(_, ref goal) => match goal {
                DrivingGoal::ParkNear(b) => TripEndpoint::Bldg(*b),
                DrivingGoal::Border(i, _) => TripEndpoint::Border(*i),
            },
            SpawnTrip::JustWalking(_, ref spot) | SpawnTrip::UsingTransit(_, ref spot, _, _, _) => {
                match spot.connection {
                    SidewalkPOI::Building(b) => TripEndpoint::Bldg(b),
                    SidewalkPOI::Border(i) => TripEndpoint::Border(i),
                    _ => unreachable!(),
                }
            }
        }
    }
}

impl PersonSpec {
    fn get_vehicles(
        &self,
        rng: &mut XorShiftRng,
    ) -> (
        Vec<VehicleSpec>,
        Vec<(usize, BuildingID)>,
        Vec<Option<usize>>,
    ) {
        let mut vehicle_specs = Vec::new();
        let mut cars_initially_parked_at = Vec::new();
        let mut vehicle_foreach_trip = Vec::new();

        let mut bike_idx = None;
        // For each indexed car, is it parked somewhere, or off-map?
        let mut car_locations: Vec<(usize, Option<BuildingID>)> = Vec::new();

        for trip in &self.trips {
            let use_for_trip = match trip.trip {
                SpawnTrip::VehicleAppearing {
                    is_bike, ref goal, ..
                }
                | SpawnTrip::FromBorder {
                    is_bike, ref goal, ..
                } => {
                    if is_bike {
                        if bike_idx.is_none() {
                            bike_idx = Some(vehicle_specs.len());
                            vehicle_specs.push(Scenario::rand_bike(rng));
                        }
                        bike_idx
                    } else {
                        // Any available cars off-map?
                        let idx = if let Some(idx) = car_locations
                            .iter()
                            .find(|(_, parked_at)| parked_at.is_none())
                            .map(|(idx, _)| *idx)
                        {
                            idx
                        } else {
                            // Need a new car, starting off-map
                            let idx = vehicle_specs.len();
                            vehicle_specs.push(Scenario::rand_car(rng));
                            idx
                        };

                        // Where does this car wind up?
                        car_locations.retain(|(i, _)| idx != *i);
                        match goal {
                            DrivingGoal::ParkNear(b) => {
                                car_locations.push((idx, Some(*b)));
                            }
                            DrivingGoal::Border(_, _) => {
                                car_locations.push((idx, None));
                            }
                        }

                        Some(idx)
                    }
                }
                SpawnTrip::UsingParkedCar(b, ref goal) => {
                    // Is there already a car parked here?
                    let idx = if let Some(idx) = car_locations
                        .iter()
                        .find(|(_, parked_at)| *parked_at == Some(b))
                        .map(|(idx, _)| *idx)
                    {
                        idx
                    } else {
                        // Need a new car, starting at this building
                        let idx = vehicle_specs.len();
                        vehicle_specs.push(Scenario::rand_car(rng));
                        cars_initially_parked_at.push((idx, b));
                        idx
                    };

                    // Where does this car wind up?
                    car_locations.retain(|(i, _)| idx != *i);
                    match goal {
                        DrivingGoal::ParkNear(b) => {
                            car_locations.push((idx, Some(*b)));
                        }
                        DrivingGoal::Border(_, _) => {
                            car_locations.push((idx, None));
                        }
                    }

                    Some(idx)
                }
                SpawnTrip::UsingBike(_, _) => {
                    if bike_idx.is_none() {
                        bike_idx = Some(vehicle_specs.len());
                        vehicle_specs.push(Scenario::rand_bike(rng));
                    }
                    bike_idx
                }
                SpawnTrip::JustWalking(_, _) | SpawnTrip::UsingTransit(_, _, _, _, _) => None,
            };
            vehicle_foreach_trip.push(use_for_trip);
        }

        (
            vehicle_specs,
            cars_initially_parked_at,
            vehicle_foreach_trip,
        )
    }
}
