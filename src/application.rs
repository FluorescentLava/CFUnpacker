//! A native Rust subclass of the WinUI 3 Application runtime class.
//!
//! The COM composition layout follows the MIT/Apache-2.0 implementation in
//! naui-windows 0.16.0 (src/app.rs), adapted to this application's launch callback.

/*
MIT License — Copyright (c) 2026 mokuichi147

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
*/

use std::{ffi::c_void, ptr::NonNull};

use naui_winui3::Microsoft::UI::Xaml::Markup::{
    IXamlMetadataProvider, IXamlMetadataProvider_Impl, IXamlMetadataProvider_Vtbl, IXamlType,
    XmlnsDefinition,
};
use naui_winui3::Microsoft::UI::Xaml::XamlTypeInfo::XamlControlsXamlMetaDataProvider;
use naui_winui3::Microsoft::UI::Xaml::{
    Application, IApplicationFactory, IApplicationOverrides, IApplicationOverrides_Impl,
    IApplicationOverrides_Vtbl, LaunchActivatedEventArgs,
};
use naui_winui3::{ChildClass, Compose, CreateInstanceFn};
use windows_core::{
    Array, ComObject, ComObjectInner, ComObjectInterface, GUID, HRESULT, HSTRING, IInspectable,
    IInspectable_Vtbl, IUnknown, IUnknownImpl, Interface, InterfaceRef, Ref,
};

struct App {
    provider: XamlControlsXamlMetaDataProvider,
    on_launched: Box<dyn Fn() -> windows_core::Result<()>>,
}

#[repr(C)]
struct AppImpl {
    identity: &'static IInspectable_Vtbl,
    overrides: &'static IApplicationOverrides_Vtbl,
    metadata: &'static IXamlMetadataProvider_Vtbl,
    this: App,
    count: windows_core::imp::WeakRefCount,
}

impl IApplicationOverrides_Impl for AppImpl {
    fn OnLaunched(&self, _args: Ref<'_, LaunchActivatedEventArgs>) -> windows_core::Result<()> {
        (self.this.on_launched)()
    }
}

impl IXamlMetadataProvider_Impl for AppImpl {
    fn GetXamlType(
        &self,
        name: &naui_winui3::Windows::UI::Xaml::Interop::TypeName,
    ) -> windows_core::Result<IXamlType> {
        self.this.provider.GetXamlType(name)
    }

    fn GetXamlTypeByFullName(&self, name: &HSTRING) -> windows_core::Result<IXamlType> {
        self.this.provider.GetXamlTypeByFullName(name)
    }

    fn GetXmlnsDefinitions(&self) -> windows_core::Result<Array<XmlnsDefinition>> {
        self.this.provider.GetXmlnsDefinitions()
    }
}

impl AppImpl {
    const IDENTITY: IInspectable_Vtbl = IInspectable_Vtbl::new::<Self, Application, 0>();
    const OVERRIDES: IApplicationOverrides_Vtbl = IApplicationOverrides_Vtbl::new::<Self, -1>();
    const METADATA: IXamlMetadataProvider_Vtbl = IXamlMetadataProvider_Vtbl::new::<Self, -2>();

    fn new(this: App) -> Self {
        Self {
            identity: &Self::IDENTITY,
            overrides: &Self::OVERRIDES,
            metadata: &Self::METADATA,
            this,
            count: windows_core::imp::WeakRefCount::new(),
        }
    }
}

impl IUnknownImpl for AppImpl {
    type Impl = App;

    fn get_impl(&self) -> &App {
        &self.this
    }

    fn get_impl_mut(&mut self) -> &mut App {
        &mut self.this
    }

    fn into_inner(self) -> App {
        self.this
    }

    fn AddRef(&self) -> u32 {
        self.count.add_ref()
    }

    unsafe fn Release(this: *mut Self) -> u32 {
        // SAFETY: COM calls Release only on a live object allocated by into_object.
        let remaining = unsafe { (*this).count.release() };
        if remaining == 0 {
            unsafe {
                drop(Box::from_raw(this));
            }
        }
        remaining
    }

    fn is_reference_count_one(&self) -> bool {
        self.count.is_one()
    }

    fn to_object(&self) -> ComObject<App> {
        self.AddRef();
        // SAFETY: the added reference keeps the same allocation alive.
        unsafe { ComObject::from_raw(NonNull::from(self)) }
    }

    unsafe fn GetTrustLevel(&self, value: *mut i32) -> HRESULT {
        if value.is_null() {
            return HRESULT(0x80004003u32 as i32);
        }
        unsafe {
            *value = 0;
        }
        HRESULT(0)
    }

    unsafe fn QueryInterface(&self, iid: *const GUID, output: *mut *mut c_void) -> HRESULT {
        if iid.is_null() || output.is_null() {
            return HRESULT(0x80004003u32 as i32);
        }
        let iid = unsafe { *iid };
        let interface = if iid == IInspectable::IID || iid == IUnknown::IID {
            &self.identity as *const _ as *const c_void
        } else if IApplicationOverrides_Vtbl::matches(&iid) {
            &self.overrides as *const _ as *const c_void
        } else if IXamlMetadataProvider_Vtbl::matches(&iid) {
            &self.metadata as *const _ as *const c_void
        } else if iid == <windows_core::imp::IMarshal as Interface>::IID {
            return unsafe {
                windows_core::imp::marshaler(self.to_interface::<IInspectable>().into(), output)
            };
        } else {
            std::ptr::null()
        };
        unsafe {
            *output = interface.cast_mut();
        }
        if interface.is_null() {
            HRESULT(0x80004002u32 as i32)
        } else {
            self.AddRef();
            HRESULT(0)
        }
    }
}

// Each interface is a pointer-sized vtable slot at the beginning of AppImpl.
impl ComObjectInterface<IInspectable> for AppImpl {
    fn as_interface_ref(&self) -> InterfaceRef<'_, IInspectable> {
        unsafe { std::mem::transmute(&self.identity) }
    }
}

impl ComObjectInterface<IUnknown> for AppImpl {
    fn as_interface_ref(&self) -> InterfaceRef<'_, IUnknown> {
        unsafe { std::mem::transmute(&self.identity) }
    }
}

impl ComObjectInterface<IApplicationOverrides> for AppImpl {
    fn as_interface_ref(&self) -> InterfaceRef<'_, IApplicationOverrides> {
        unsafe { std::mem::transmute(&self.overrides) }
    }
}

impl ComObjectInterface<IXamlMetadataProvider> for AppImpl {
    fn as_interface_ref(&self) -> InterfaceRef<'_, IXamlMetadataProvider> {
        unsafe { std::mem::transmute(&self.metadata) }
    }
}

impl ComObjectInner for App {
    type Outer = AppImpl;

    fn into_object(self) -> ComObject<Self> {
        let boxed = Box::new(AppImpl::new(self));
        unsafe { ComObject::from_raw(Box::into_non_null(boxed)) }
    }
}

impl From<App> for IInspectable {
    fn from(app: App) -> Self {
        ComObject::new(app).into_interface()
    }
}

impl ChildClass for App {
    type BaseType = Application;
    type FactoryInterface = IApplicationFactory;

    fn create_interface_fn(
        vtable: &<Self::FactoryInterface as Interface>::Vtable,
    ) -> CreateInstanceFn {
        vtable.CreateInstance
    }

    fn identity_vtable(outer: &mut Self::Outer) -> &mut &'static IInspectable_Vtbl {
        &mut outer.identity
    }

    fn ref_count(outer: &Self::Outer) -> &windows_core::imp::WeakRefCount {
        &outer.count
    }

    fn into_outer(self) -> Self::Outer {
        AppImpl::new(self)
    }
}

/// Create the Application from inside Application::Start's initialization callback.
/// The caller may append XamlControlsResources and custom dictionaries to Resources.
pub fn create(
    on_launched: impl Fn() -> windows_core::Result<()> + 'static,
) -> windows_core::Result<Application> {
    XamlControlsXamlMetaDataProvider::Initialize()?;
    Compose::compose(App {
        provider: XamlControlsXamlMetaDataProvider::new()?,
        on_launched: Box::new(on_launched),
    })
}
