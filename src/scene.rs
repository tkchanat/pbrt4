//! Scene loader

use std::{collections::HashMap, env, fs, path::Path, slice, str};

use glam::{Mat4, Vec3};

use crate::{
    param::ParamList,
    types::{
        Accelerator, AreaLight, Camera, Film, Integrator, Light, Material, Medium, Options,
        PixelFilter, Sampler, Shape, Texture,
    },
    Element, Error, Parser, Result,
};

/// A number of directives modify the current graphics state.
/// Examples include the transformation directives (Transformations),
/// and the directive that sets the current material.
#[derive(Default, Clone)]
struct State<'a> {
    /// The reverse-orientation setting, specified by the `ReverseOrientation`
    /// directive, is part of the graphics state.
    reverse_orientation: bool,

    transform_matrix: Mat4,

    current_inside_medium: Option<&'a str>,
    current_outside_medium: Option<&'a str>,

    material_index: Option<usize>,
    area_light_index: Option<usize>,

    /// Between `ObjectBegin` and `ObjectEnd` if `Some`.
    active_object: Option<usize>,
    shape_count: usize,

    shape_params: ParamList<'a>,
    light_params: ParamList<'a>,
    material_params: ParamList<'a>,
    medium_params: ParamList<'a>,
    texture_params: ParamList<'a>,
}

#[derive(Debug)]
pub struct CameraEntity {
    pub params: Camera,
    pub transform: Mat4,
}

#[derive(Debug)]
pub struct ShapeEntity {
    pub params: Shape,
    /// If shape is a part of [Object], transform matrix defines the transformation from
    /// object space to the instance's coordinate space.
    pub transform: Mat4,
    pub reverse_orientation: bool,
    pub material_index: Option<usize>,
    pub area_light_index: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Object {
    pub name: String,
    pub shape_start: Option<usize>,
    pub shape_count: usize,
    pub object_to_instance: Mat4,
}

#[derive(Debug)]
pub struct Instance {
    pub instance_to_world: Mat4,
    pub object_index: usize,
    pub area_light_index: Option<usize>,
    pub reverse_orientation: bool,
}

#[derive(Default)]
pub struct Scene {
    pub start_time: f32,
    pub end_time: f32,
    pub options: Options,
    pub camera: Option<CameraEntity>,
    pub film: Option<Film>,
    pub integrator: Option<Integrator>,
    pub pixel_filter: Option<PixelFilter>,
    pub accelerator: Option<Accelerator>,
    pub sampler: Option<Sampler>,
    pub textures: Vec<Texture>,
    pub materials: Vec<Material>,
    pub lights: Vec<Light>,
    pub area_lights: Vec<AreaLight>,
    pub mediums: Vec<Medium>,
    pub shapes: Vec<ShapeEntity>,
    pub objects: Vec<Object>,
    pub instances: Vec<Instance>,
}

impl Scene {
    /// Load a scene from a file at path.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Scene> {
        let path = path.as_ref();

        let working_directory = path.parent();

        let data = fs::read_to_string(path)?;
        Self::load(&data, working_directory)
    }

    /// Load a PBRT v4 scene from a string slice.
    ///
    /// # Arguments
    /// - `data` is a string buffer with the file data.
    /// - `working_directory` is a file's directory path which required for includes
    /// with relative paths to work.
    pub fn load(data: &str, working_directory: Option<&Path>) -> Result<Scene> {
        let mut scene = Scene::default();

        let mut parsers = Vec::new();
        parsers.push(Parser::new(data));

        let mut current_state = State::default();
        let mut states_stack = Vec::new();
        let mut is_world_block = false;

        let mut named_coord_systems: HashMap<String, Mat4> = HashMap::default();

        // Texture name to index.
        let mut named_textures: HashMap<String, usize> = HashMap::default();
        let mut named_materials: HashMap<String, usize> = HashMap::default();
        let mut named_mediums: HashMap<String, usize> = HashMap::default();
        let mut named_objects: HashMap<String, usize> = HashMap::default();

        // Because data from included files might end up in cached parameters,
        // we should keep the file data around until scene loading is done.
        let mut includes = Vec::new();

        while let Some(parser) = parsers.last_mut() {
            // Fetch next element.
            let element = match parser.parse_next() {
                Ok(element) => element,
                Err(err) if matches!(err, Error::EndOfFile) => {
                    // Remove parser from the stack.
                    parsers.pop();
                    continue;
                }
                Err(err) => return Err(err),
            };

            match element {
                Element::AttributeBegin => {
                    states_stack.push(current_state.clone());
                }
                Element::AttributeEnd => match states_stack.pop() {
                    Some(state) => current_state = state,
                    None => return Err(Error::TooManyEndAttributes),
                },
                Element::Attribute { target, params } => match target {
                    "shape" => current_state.shape_params.extend(&params),
                    "light" => current_state.light_params.extend(&params),
                    "material" => current_state.material_params.extend(&params),
                    "medium" => current_state.medium_params.extend(&params),
                    "texture" => current_state.texture_params.extend(&params),
                    _ => unimplemented!(),
                },
                Element::ReverseOrientation => {
                    current_state.reverse_orientation = !current_state.reverse_orientation;
                }
                Element::Translate { v } => {
                    current_state.transform_matrix *= Mat4::from_translation(Vec3::from(v))
                }
                Element::Identity => {
                    current_state.transform_matrix = Mat4::IDENTITY;
                }
                // Transform resets the CTM to the specified matrix.
                Element::Transform { m } => {
                    current_state.transform_matrix = Mat4::from_cols_array(&m);
                }
                // An arbitrary transformation to multiply the CTM with can be specified using ConcatTransform
                Element::ConcatTransform { m } => {
                    current_state.transform_matrix *= Mat4::from_cols_array(&m);
                }
                Element::Scale { v } => {
                    current_state.transform_matrix *= Mat4::from_scale(Vec3::from(v));
                }
                Element::Rotate { angle, v } => {
                    current_state.transform_matrix *= Mat4::from_axis_angle(Vec3::from(v), angle);
                }
                Element::LookAt { eye, look_at, up } => {
                    current_state.transform_matrix *=
                        Mat4::look_at_lh(Vec3::from(eye), Vec3::from(look_at), Vec3::from(up));
                }
                // A name can be associated with the CTM using the CoordinateSystem directive.
                Element::CoordinateSystem { name } => {
                    named_coord_systems.insert(name.to_string(), current_state.transform_matrix);
                }
                // The CTM can later be reset to the recorded transformation using CoordSysTransform.
                Element::CoordSysTransform { name } => {
                    match named_coord_systems.get(name).copied() {
                        Some(mat) => current_state.transform_matrix = mat,
                        None => {
                            // TODO: Material not found, return error.
                            unimplemented!()
                        }
                    }
                }
                // The Camera directive specifies the camera used for viewing the scene.
                Element::Camera { ty, params } => {
                    let camera_from_world = current_state.transform_matrix;
                    // TODO: Support transformStartTime and transformEndTime
                    let world_from_camera = camera_from_world.inverse();

                    // pbrt automatically records the camera transformation matrix in the "camera" named coordinate system.
                    // This can be useful for placing light sources with respect to the camera, for example.

                    // TODO: Fix key
                    named_coord_systems.insert("camera".to_string(), world_from_camera);

                    let camera = Camera::new(ty, params)?;

                    let entity = CameraEntity {
                        params: camera,
                        transform: world_from_camera,
                    };

                    scene.camera = Some(entity);
                }
                Element::Film { ty, params } => {
                    debug_assert!(scene.film.is_none());
                    let film = Film::new(ty, params)?;
                    scene.film = Some(film);
                }
                Element::Integrator { ty, params } => {
                    debug_assert!(scene.integrator.is_none());
                    let integrator = Integrator::new(ty, params)?;
                    scene.integrator = Some(integrator);
                }
                Element::Accelerator { ty, params } => {
                    debug_assert!(scene.accelerator.is_none());
                    let accelerator = Accelerator::new(ty, params)?;
                    scene.accelerator = Some(accelerator);
                }
                Element::PixelFilter { ty, params } => {
                    debug_assert!(scene.pixel_filter.is_none());
                    let filter = PixelFilter::new(ty, params)?;
                    scene.pixel_filter = Some(filter);
                }
                Element::ColorSpace { .. } => {
                    todo!("Support color space");
                }
                Element::Sampler { ty, params } => {
                    let sampler = Sampler::new(ty, params)?;

                    debug_assert!(scene.sampler.is_none());
                    scene.sampler = Some(sampler);
                }
                // pbrt supports animated transformations by allowing two transformation
                // matrices to be specified at different times.
                Element::TransformTimes { start, end } => {
                    // TransformTimes directive must be outside of the world definition block,
                    if is_world_block {
                        return Err(Error::WorldAlreadyStarted);
                    }

                    scene.start_time = start;
                    scene.end_time = end;
                }
                // ActiveTransform directive indicates whether subsequent directives that modify the CTM should
                // apply to the transformation at the starting time, the transformation at the ending time, or both.
                Element::ActiveTransform { .. } => {
                    todo!("Support animated transformations")
                }
                // Include behaves similarly to the #include directive in C++: parsing of the current file is suspended,
                // the specified file is parsed in its entirety, and only then does parsing of the current file resume.
                // Its effect is equivalent to direct text substitution of the included file.
                Element::Include(path) => {
                    // If the filename given to a Include or Import statement is not an absolute path,
                    // its path is interpreted as being relative to the directory of the initial file being parsed as
                    // specified with pbrt's command-line arguments.
                    let path = Path::new(path);

                    let full_path;

                    let path = if path.is_absolute() {
                        path
                    } else {
                        full_path = match working_directory {
                            Some(directory) => directory.join(path),
                            // Use current working directory if not provided
                            None => env::current_dir()?.join(path),
                        };

                        full_path.as_path()
                    };

                    let data = fs::read_to_string(path)?;

                    // Included files may be compressed using gzip.
                    // If a scene file name has a ".gz" suffix, then pbrt will automatically decompress it as it is read from disk.
                    if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
                        if ext.ends_with(".gz") {
                            todo!("Gzip compression");
                        }
                    }

                    // In Rust, String is heap allocated type, so it's safe to keep a pointer to
                    // the raw data and move the String object (like push it to the vector).
                    let raw = data.as_bytes();
                    let raw_len = raw.len();
                    let raw_ptr = raw.as_ptr();

                    includes.push(data);

                    // TODO: is there a better way?
                    let parser = Parser::new(unsafe {
                        let byte_slice = slice::from_raw_parts(raw_ptr, raw_len);
                        str::from_utf8_unchecked(byte_slice)
                    });
                    parsers.push(parser);
                }
                Element::Import(..) => {
                    todo!("Support imports")
                }
                Element::WorldBegin => {
                    is_world_block = true;
                    current_state.transform_matrix = Mat4::IDENTITY;
                }
                Element::Option(param) => {
                    scene.options.apply(param)?;
                }
                Element::Texture {
                    name,
                    ty,
                    class,
                    mut params,
                } => {
                    params.extend(&current_state.texture_params);
                    let texture = Texture::new(name, ty, class, params)?;

                    let index = scene.textures.len();
                    scene.textures.push(texture);

                    named_textures.insert(name.to_string(), index);
                }
                // The Material directive specifies the current material, which then applies for all subsequent
                // shape definitions (until the end of the current attribute scope or until a new material is defined.
                Element::Material { ty, mut params } => {
                    params.extend(&current_state.material_params);
                    let material = Material::new(ty, params, &named_textures)?;

                    let index = scene.materials.len();
                    scene.materials.push(material);

                    current_state.material_index = Some(index);
                }
                Element::MakeNamedMaterial { name, mut params } => {
                    params.extend(&current_state.material_params);
                    let material = Material::new(name, params, &named_textures)?;

                    let index = scene.materials.len();
                    scene.materials.push(material);

                    named_materials.insert(name.to_string(), index);
                }
                Element::NamedMaterial { name } => {
                    // TODO: handle material not found case.
                    current_state.material_index = named_materials.get(name).copied();
                }
                Element::LightSource { ty, params } => {
                    // When a light source is created, the current exterior medium is used for rays leaving the light
                    // when bidirectional light transport algorithms are used.
                    //
                    // The user is responsible for specifying media in a way such that rays reaching lights are in the same medium
                    // as rays leaving those lights.

                    // TODO: Handle current_outside_medium

                    let light = Light::new(ty, params)?;
                    scene.lights.push(light);
                }
                // After an AreaLightSource directive, all subsequent shapes emit light
                // from their surfaces according to the distribution defined by the given
                // area light implementation.
                Element::AreaLightSource { ty, mut params } => {
                    params.extend(&current_state.light_params);
                    let area_light = AreaLight::new(ty, params)?;

                    let index = scene.area_lights.len();
                    scene.area_lights.push(area_light);

                    // The current area light is saved and restored inside attribute blocks;
                    // typically area light definitions are inside an AttributeBegin/AttributeEnd
                    // pair in order to control the shapes that they are applied to.
                    current_state.area_light_index = Some(index);
                }
                Element::Shape {
                    name: ty,
                    mut params,
                } => {
                    params.extend(&current_state.shape_params);
                    let shape = Shape::new(ty, params)?;

                    // When a shape is created, the current interior medium is assumed to be the medium inside the shape,
                    // and the current exterior medium is assumed to be the medium outside the shape.
                    // TODO: handle mediums

                    let entity = ShapeEntity {
                        params: shape,
                        transform: current_state.transform_matrix,
                        reverse_orientation: current_state.reverse_orientation,
                        material_index: current_state.material_index,
                        area_light_index: current_state.area_light_index,
                    };

                    scene.shapes.push(entity);

                    // If inside of ObjectBegin/ObjectEnd, count the number of shapes.
                    if current_state.active_object.is_some() {
                        current_state.shape_count += 1;
                    }
                }
                Element::ObjectBegin { name } => {
                    if current_state.active_object.is_some() {
                        // Nested objects are not allowed
                        return Err(Error::NestedObjects);
                    }

                    states_stack.push(current_state.clone());

                    let object = Object {
                        name: name.to_string(),
                        shape_start: None,
                        shape_count: 0,
                        object_to_instance: current_state.transform_matrix,
                    };

                    let index = scene.objects.len();
                    scene.objects.push(object);

                    current_state.active_object = Some(index);
                    named_objects.insert(name.to_string(), index);
                }
                Element::ObjectEnd => {
                    let object_index = current_state
                        .active_object
                        .take()
                        .ok_or(Error::ElementNotAllowed)?;

                    let object = &mut scene.objects[object_index];

                    object.shape_count = current_state.shape_count;

                    if object.shape_count > 0 {
                        object.shape_start = Some(scene.shapes.len() - object.shape_count)
                    }

                    current_state.shape_count = 0;
                    current_state.active_object = None;

                    match states_stack.pop() {
                        Some(state) => current_state = state,
                        None => return Err(Error::ElementNotAllowed),
                    }
                }
                Element::ObjectInstance { name } => {
                    let Some(object_index) = named_objects.get(name).copied() else {
                        return Err(Error::NotFound)
                    };

                    let instance = Instance {
                        // The current transformation matrix defines the world from instance space transformation.
                        instance_to_world: current_state.transform_matrix,
                        object_index,
                        area_light_index: current_state.area_light_index,
                        reverse_orientation: current_state.reverse_orientation,
                    };

                    scene.instances.push(instance);
                }
                // MakeNamedMedium associates a user-specified name with medium scattering characteristics.
                Element::MakeNamedMedium { name, mut params } => {
                    params.extend(&current_state.medium_params);
                    let medium = Medium::new(params)?;

                    let index = scene.mediums.len();
                    scene.mediums.push(medium);

                    named_mediums.insert(name.to_string(), index);
                }
                // MediumInterface directive can be used to specify the current "interior" and "exterior" media.
                // A vacuum—no participating media—is represented by empty string "".
                Element::MediumInterface { interior, exterior } => {
                    current_state.current_inside_medium = Some(interior);
                    current_state.current_outside_medium = Some(exterior);
                }
            }
        }

        debug_assert!(states_stack.is_empty());
        debug_assert!(is_world_block);

        Ok(scene)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempdir::TempDir;

    #[test]
    fn test_includes() -> Result<()> {
        let temp_dir = TempDir::new("pbrt-includes-")?;
        let temp_path = temp_dir.path();

        fs::write(temp_path.join("1.pbrt"), "Shape \"sphere\"")?;
        fs::write(temp_path.join("2.pbrt"), "Include \"1.pbrt\" ")?;
        fs::write(temp_path.join("3.pbrt"), "Include \"2.pbrt\" ")?;
        fs::write(temp_path.join("4.pbrt"), "Include \"3.pbrt\" ")?;

        fs::write(
            temp_path.join("main.pbrt"),
            r#"
WorldBegin

Include "4.pbrt" # Include file with nexted includes
Include "1.pbrt" # Include shap directly

        "#,
        )?;

        let scene = Scene::from_file(temp_path.join("main.pbrt"))?;

        debug_assert_eq!(scene.shapes.len(), 2);

        Ok(())
    }

    #[test]
    fn test_instancing() -> Result<()> {
        let data = r#"
WorldBegin

ObjectBegin "foo"
Shape "sphere"
Shape "sphere"
ObjectEnd

ObjectInstance "foo"
Translate 1 0 0
ObjectInstance "foo"
        "#;

        let scene = Scene::load(data, None)?;

        assert_eq!(scene.shapes.len(), 2);

        assert!(matches!(scene.shapes[0].params, Shape::Sphere { .. }));
        assert!(matches!(scene.shapes[0].params, Shape::Sphere { .. }));

        assert_eq!(scene.objects.len(), 1);

        let object = &scene.objects[0];
        assert_eq!(&object.name, "foo");
        assert_eq!(object.shape_start, Some(0));
        assert_eq!(object.shape_count, 2);

        assert_eq!(scene.instances.len(), 2);

        let inst1 = &scene.instances[0];
        assert_eq!(inst1.object_index, 0);

        let inst2 = &scene.instances[1];
        assert_eq!(inst1.object_index, 0);
        assert_ne!(inst1.instance_to_world, inst2.instance_to_world);

        Ok(())
    }
}
