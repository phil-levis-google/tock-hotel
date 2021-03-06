#![allow(dead_code)]

mod constants;
mod registers;
mod serialize;
mod types;

use cortexm3::support;

pub use self::constants::Descriptor;
pub use self::registers::DMADescriptor;
pub use self::types::StringDescriptor;

use core::cell::Cell;
use kernel::common::cells::TakeCell;
use pmu::{Clock, PeripheralClock, PeripheralClock1};

use self::constants::*;
use self::registers::{EpCtl, DescFlag, Registers};
use self::types::{StaticRef};
use self::types::{SetupRequest, SetupRequestType};
use self::types::{SetupDirection, SetupRequestClass, SetupRecipient};
use self::types::{DeviceDescriptor, ConfigurationDescriptor};
use self::types::{InterfaceDescriptor, EndpointDescriptor, HidDeviceDescriptor};
use self::types::{EndpointAttributes, EndpointUsageType, EndpointTransferType};
use self::types::{EndpointSynchronizationType};

// Simple macro for USB debugging output: default definitions do nothing,
// but you can uncomment print defintions to get detailed output on the
// messages sent and received.
macro_rules! usb_debug {
//    () => ({print!();});
//    ($fmt:expr) => ({print!($fmt);});
//    ($fmt:expr, $($arg:tt)+) => ({print!($fmt, $($arg)+);});
    () => ({});
    ($fmt:expr) => ({});
    ($fmt:expr, $($arg:tt)+) => ({});
}


/// USBState encodes the current state of the USB driver's state
/// machine. It can be in three states: waiting for a message from
/// the host, sending data in reply to a query from the host, or sending
/// a status response (no data) in reply to a command from the host.
#[derive(Clone, Copy, PartialEq, Eq)]
enum USBState {
    WaitingForSetupPacket,   // Waiting for message from host
    DataStageIn,             // Sending data to host
    NoDataStage,             // Sending status (not data) to host,
                             // e.g. in response to set command
}

/// Driver for the Synopsys DesignWare Cores USB 2.0 Hi-Speed
/// On-The-Go (OTG) controller.
///
/// Page/figure references are for the Synopsys DesignWare Cores USB
/// 2.0 Hi-Speed On-The-Go (OTG) Programmer's Guide.
///
/// The driver can enumerate (appear as a device to a host OS) but
/// cannot perform any other operations (yet). The driver operates as
/// a device in Scatter-Gather DMA mode (Figure 1-1) and performs the
/// initial handshakes with the host on endpoint 0. It appears as an
/// "Unknown counterfeit flash drive" (ID 0011:7788) under Linux; this
/// was chosen as it won't collide with other valid devices and Linux
/// doesn't expect anything.
///
/// Scatter-gather mode operates using lists of descriptors. Each
/// descriptor points to a 64 byte memory buffer. A transfer larger
/// than 64 bytes uses multiple descriptors in sequence. An IN
/// descriptor is for sending to the host (the data goes IN to the
/// host), while an OUT descriptor is for receiving from the host (the
/// data goes OUT of the host).
///
/// For endpoint 0, the driver configures 2 OUT descriptors and 4 IN
/// descriptors. Four IN descriptors allows responses up to 256 bytes
/// (64 * 4), which is important for sending the device configuration
/// descriptor as one big blob.  The driver never expects to receive
/// OUT packets larger than 64 bytes (the maximum each descriptor can
/// handle). It uses two OUT descriptors so it can receive a packet
/// while processing the previous one.
///
/// The USB stack currently assumes the presence of 7
/// StringDescriptors, which are provided by the boot sequence. The
/// meaning of each StringDescriptor is defined by its index, in
/// usb::constants.

pub struct USB {
    registers: StaticRef<Registers>,

    core_clock: Clock,
    timer_clock: Clock,

    // Current state of the driver
    state: Cell<USBState>,

    // Descriptor and buffers should never be empty after a call
    // to init.
    ep0_out_descriptors: TakeCell<'static, [DMADescriptor; 2]>,
    ep0_out_buffers: Cell<Option<&'static [[u32; 16]; 2]>>,
    ep0_in_descriptors: TakeCell<'static, [DMADescriptor; 4]>,
    // `ep0_in_buffers` is one large buffer so we can copy into it as
    // one big blob; `ep0_in_descriptors` can point into the middle of
    // this buffer.
    ep0_in_buffers: TakeCell<'static, [u32; 16 * 4]>,

    // Track the index of which ep0_out descriptor is currently set
    // for reception and which descriptor received the most
    // recent packet.
    next_out_idx: Cell<usize>,
    last_out_idx: Cell<usize>,

    device_class: Cell<u8>,
    vendor_id: Cell<u16>,
    product_id: Cell<u16>,

    // `configuration_descriptor` stores the bytes of the full
    // ConfigurationDescriptor, whose length is stored in
    // `configuration_total_length`.  The field is populated by
    // serializing all of the descriptors into it. Currently limited
    // to a single 64 byte buffer.
    configuration_descriptor: TakeCell<'static, [u8; 64]>,
    configuration_total_length: Cell<u16>,
    // Which configuration is currently being used.
    configuration_current_value: Cell<u8>,
    strings: TakeCell<'static, [StringDescriptor]>,
}

// Hardware base address of the singleton USB controller
const BASE_ADDR: *const Registers = 0x40300000 as *const Registers;
pub static mut USB0: USB = unsafe { USB::new() };

// Statically allocated buffers for initializing USB stack
pub static mut OUT_DESCRIPTORS: [DMADescriptor; 2] = [DMADescriptor {
    flags: DescFlag::HOST_BUSY,
    addr: 0,
}; 2];
pub static mut OUT_BUFFERS: [[u32; 16]; 2] = [[0; 16]; 2];
pub static mut IN_DESCRIPTORS: [DMADescriptor; 4] = [DMADescriptor {
    flags: DescFlag::HOST_BUSY,
    addr: 0,
}; 4];
pub static mut IN_BUFFERS: [u32; 16 * 4] = [0; 16 * 4];
pub static mut CONFIGURATION_BUFFER: [u8; 64] = [0; 64];

impl USB {
    /// Creates a new value referencing the single USB driver.
    ///
    /// ## Safety
    ///
    /// Callers must ensure this is only called once for every program
    /// execution. Creating multiple instances will result in conflicting
    /// handling of events and can lead to undefined behavior.
    const unsafe fn new() -> USB {
        USB {
            registers: StaticRef::new(BASE_ADDR),
            core_clock: Clock::new(PeripheralClock::Bank1(PeripheralClock1::Usb0)),
            timer_clock: Clock::new(PeripheralClock::Bank1(PeripheralClock1::Usb0TimerHs)),
            state: Cell::new(USBState::WaitingForSetupPacket),
            ep0_out_descriptors: TakeCell::empty(),
            ep0_out_buffers: Cell::new(None),
            ep0_in_descriptors: TakeCell::empty(),
            ep0_in_buffers: TakeCell::empty(),
            configuration_descriptor: TakeCell::empty(),
            next_out_idx: Cell::new(0),
            last_out_idx: Cell::new(0),
            device_class: Cell::new(0x00),
            vendor_id: Cell::new(0x0011),    // Unknown
            product_id: Cell::new(0x5026),   // unknown counterfeit flash drive
            configuration_current_value: Cell::new(0),
            configuration_total_length: Cell::new(0),
            strings: TakeCell::empty(),
        }
    }

    /// Initialize the USB driver in device mode, so it can be begin
    /// communicating with a connected host.
    pub fn init(&self,
                out_descriptors: &'static mut [DMADescriptor; 2],
                out_buffers: &'static mut [[u32; 16]; 2],
                in_descriptors: &'static mut [DMADescriptor; 4],
                in_buffers: &'static mut [u32; 16 * 4],
                configuration_buffer: &'static mut [u8; 64],
                phy: PHY,
                device_class: Option<u8>,
                vendor_id: Option<u16>,
                product_id: Option<u16>,
                strings: &'static mut [StringDescriptor]) {
        self.ep0_out_descriptors.replace(out_descriptors);
        self.ep0_out_buffers.set(Some(out_buffers));
        self.ep0_in_descriptors.replace(in_descriptors);
        self.ep0_in_buffers.replace(in_buffers);
        self.configuration_descriptor.replace(configuration_buffer);
        self.strings.replace(strings);
        
        if let Some(dclass) = device_class {
            self.device_class.set(dclass);
        }

        if let Some(vid) = vendor_id {
            self.vendor_id.set(vid);
        }

        if let Some(pid) = product_id {
            self.product_id.set(pid);
        }

        self.generate_full_configuration_descriptor();
        
        self.core_clock.enable();
        self.timer_clock.enable();

        self.registers.interrupt_mask.set(0);
        self.registers.device_all_ep_interrupt_mask.set(0);
        self.registers.device_in_ep_interrupt_mask.set(0);
        self.registers.device_out_ep_interrupt_mask.set(0);

        // This code below still needs significant cleanup -pal
        let sel_phy = match phy {
            PHY::A => 0b100, // USB PHY0
            PHY::B => 0b101, // USB PHY1
        };
        // Select PHY A
        self.registers.gpio.set((1 << 15 | // WRITE mode
                                sel_phy << 4 | // Select PHY A & Set PHY active
                                0) << 16); // CUSTOM_CFG Register

        // Configure the chip
        self.registers.configuration.set(1 << 6 | // USB 1.1 Full Speed
            0 << 5 | // 6-pin unidirectional
            14 << 10 | // USB Turnaround time to 14 -- what does this mean though??
            7); // Timeout calibration to 7 -- what does this mean though??


        // Soft reset
        self.soft_reset();

        // Configure the chip
        self.registers.configuration.set(1 << 6 | // USB 1.1 Full Speed
            0 << 5 | // 6-pin unidirectional
            14 << 10 | // USB Turnaround time to 14 -- what does this mean though??
            7); // Timeout calibration to 7 -- what does this mean though??

        // === Begin Core Initialization ==//

        // We should be reading `user_hw_config` registers to find out about the
        // hardware configuration (which endpoints are in/out, OTG capable,
        // etc). Skip that for now and just make whatever assumption CR50 is
        // making.

        // Set the following parameters:
        //   * Enable DMA Mode
        //   * Global unmask interrupts
        //   * Interrupt on Non-Periodic TxFIFO completely empty
        // _Don't_ set:
        //   * Periodic TxFIFO interrupt on empty (only valid in slave mode)
        //   * AHB Burst length (defaults to 1 word)
        self.registers.ahb_config.set(1 |      // Global Interrupt unmask
                                      1 << 5 | // DMA Enable
                                      1 << 7); // Non_periodic TxFIFO

        // Set Soft Disconnect bit to make sure we're in disconnected state
        self.registers.device_control.set(self.registers.device_control.get() | (1 << 1));

        // The datasheet says to unmask OTG and Mode Mismatch interrupts, but
        // we don't support anything but device mode for now, so let's skip
        // handling that
        //
        // If we're right, then
        // `self.registers.interrupt_status.get() & 1 == 0`
        //

        // === Done with core initialization ==//

        // ===  Begin Device Initialization  ==//

        self.registers.device_config.set(self.registers.device_config.get() |
            0b11       | // Device Speed: USB 1.1 Full speed (48Mhz)
            0 << 2     | // Non-zero-length Status: send packet to application
            0b00 << 11 | // Periodic frame interval: 80%
            1 << 23);   // Enable Scatter/gather

        // We would set the device threshold control register here, but I don't
        // think we enable thresholding.

        self.setup_data_fifos();

        // Clear any pending interrupts
        for endpoint in self.registers.out_endpoints.iter() {
            endpoint.interrupt.set(!0);
        }
        for endpoint in self.registers.in_endpoints.iter() {
            endpoint.interrupt.set(!0);
        }
        self.registers.interrupt_status.set(!0);

        // Unmask some endpoint interrupts
        //    Device OUT SETUP & XferCompl
        self.registers.device_out_ep_interrupt_mask.set(1 << 0 | // XferCompl
            1 << 1 | // Disabled
            1 << 3); // SETUP
        //    Device IN XferCompl & TimeOut
        self.registers.device_in_ep_interrupt_mask.set(1 << 0 | // XferCompl
            1 << 1); // Disabled

        // To set ourselves up for processing the state machine through interrupts,
        // unmask:
        //
        //   * USB Reset
        //   * Enumeration Done
        //   * Early Suspend
        //   * USB Suspend
        //   * SOF
        //
        self.registers
            .interrupt_mask
            .set(GOUTNAKEFF | GINNAKEFF | USB_RESET | ENUM_DONE | OEPINT | IEPINT |
                 EARLY_SUSPEND | USB_SUSPEND | SOF);

        // Power on programming done
        self.registers.device_control.set(self.registers.device_control.get() | 1 << 11);
        for _ in 0..10000 {
            support::nop();
        }
        self.registers.device_control.set(self.registers.device_control.get() & !(1 << 11));

        // Clear global NAKs
        self.registers.device_control.set(self.registers.device_control.get() |
            1 << 10 | // Clear global OUT NAK
            1 << 8);  // Clear Global Non-periodic IN NAK

        // Reconnect:
        //  Clear the Soft Disconnect bit to allow the core to issue a connect.
        self.registers.device_control.set(self.registers.device_control.get() & !(1 << 1));

    }


    
    /// Initialize descriptors for endpoint 0 IN and OUT, resetting
    /// the endpoint 0 descriptors to a clean state and puttingx the
    /// stack into the state of waiting for a SETUP packet from the
    /// host (since this is the first message in an enumeration
    /// exchange).
    fn init_descriptors(&self) {
        // Setup descriptor for OUT endpoint 0
        self.ep0_out_buffers.get().map(|bufs| {
            self.ep0_out_descriptors.map(|descs| {
                for (desc, buf) in descs.iter_mut().zip(bufs.iter()) {
                    desc.flags = DescFlag::HOST_BUSY;
                    desc.addr = buf.as_ptr() as usize;
                }
                self.next_out_idx.set(0);
                self.registers.out_endpoints[0].dma_address.set(&descs[0]);
            });
        });

        // Setup descriptor for IN endpoint 0
        self.ep0_in_buffers.map(|buf| {
            self.ep0_in_descriptors.map(|descs| {
                for (i, desc) in descs.iter_mut().enumerate() {
                    desc.flags = DescFlag::HOST_BUSY;
                    desc.addr = buf.as_ptr() as usize + i * 64;
                }
                self.registers.in_endpoints[0].dma_address.set(&descs[0]);
            });
        });


        self.expect_setup_packet();
    }

    /// Reset the device in response to a USB RESET.
    fn reset(&self) {
        usb_debug!("USB: WaitingForSetupPacket in reset.\n");
        self.state.set(USBState::WaitingForSetupPacket);
        // Reset device address field (bits 10:4) of device config
        //self.registers.device_config.set(self.registers.device_config.get() & !(0b1111111 << 4));

        self.init_descriptors();
    }

    /// Perform a soft reset on the USB core; timeout if the reset
    /// takes too long.
    fn soft_reset(&self) {
        // Reset
        self.registers.reset.set(Reset::CSftRst as u32);

        let mut timeout = 10000;
        // Wait until reset flag is cleared or timeout
        while self.registers.reset.get() & (Reset::CSftRst as u32) == 1 &&
            timeout > 0 {
            timeout -= 1;
        }
        if timeout == 0 {
            return;
        }

        // Wait until Idle flag is set or timeout
        let mut timeout = 10000;
        while self.registers.reset.get() & (Reset::AHBIdle as u32) == 0 &&
            timeout > 0 {
            timeout -= 1;
        }
        if timeout == 0 {
            return;
        }

    }
    
    /// The chip should call this interrupt bottom half from its
    /// `service_pending_interrupts` routine when an interrupt is
    /// received on the USB nvic line. T
    ///
    /// Directly handles events related to device initialization, connection and
    /// disconnection, as well as control transfers on endpoint 0. Other events
    /// are passed to clients delegated for particular endpoints or interfaces.
    ///
    /// TODO(alevy): implement what this comment promises
    pub fn handle_interrupt(&self) {
        // Save current interrupt status snapshot to correctly clear at end
        let status = self.registers.interrupt_status.get();
        //print_usb_interrupt_status(status);
 
        if status & ENUM_DONE != 0 {
            // MPS default set to 0 == 64 bytes
            // "Application must read the DSTS register to obtain the
            //  enumerated speed."
        }

        if status & EARLY_SUSPEND != 0  || status & USB_SUSPEND != 0 {
            // Currently do not support suspend
        }
        
        if self.registers.interrupt_mask.get() & status & SOF != 0 { // Clear SOF
            self.registers.interrupt_mask.set(self.registers.interrupt_mask.get() & !SOF);
        }

        if status & GOUTNAKEFF != 0 { // Clear Global OUT NAK
            self.registers.device_control.set(self.registers.device_control.get() | 1 << 10);
        }

        if status & GINNAKEFF != 0 { // Clear Global Non-periodic IN NAK
            self.registers.device_control.set(self.registers.device_control.get() | 1 << 8);
        }

        if status & (OEPINT | IEPINT) != 0 { // Interrupt pending
            usb_debug!(" - handling endpoint interrupts\n");
            let daint = self.registers.device_all_ep_interrupt.get();
            let inter_ep0_out = daint & 1 << 16 != 0;
            let inter_ep0_in = daint & 1 != 0;
            if inter_ep0_out || inter_ep0_in {
                self.handle_endpoint0_events(inter_ep0_out, inter_ep0_in);
            }
        }

        if status & USB_RESET != 0 {
            self.reset();
        }
        
        self.registers.interrupt_status.set(status);
    }

    /// Set up endpoint 0 OUT descriptors to receive a setup packet
    /// from the host, whose reception will trigger an interrupt.
    /// Preparing for a SETUP packet disables IN interrupts (device
    /// should not be sending anything) and enables OUT interrupts
    /// (for reception from host).
    //
    // A SETUP packet is less than 64 bytes, so only one OUT
    // descriptor is needed. This function sets the max size of the
    // packet to 64 bytes the Last and Interrupt-on-completion bits
    // and max size to 64 bytes.
    fn expect_setup_packet(&self) {
        usb_debug!("USB: WaitingForSetupPacket in expect_setup_packet.\n");
        self.state.set(USBState::WaitingForSetupPacket);
        self.ep0_out_descriptors.map(|descs| {
            descs[self.next_out_idx.get()].flags =
                (DescFlag::HOST_READY | DescFlag::LAST | DescFlag::IOC).bytes(64);
        });

        // Enable OUT and disable IN interrupts
        let mut interrupts = self.registers.device_all_ep_interrupt_mask.get();
        interrupts |= AllEndpointInterruptMask::OUT0 as u32;
        interrupts &= !(AllEndpointInterruptMask::IN0 as u32);
        self.registers.device_all_ep_interrupt_mask.set(interrupts);

        // Clearing the NAK bit tells host that device is ready to receive.
        self.registers.out_endpoints[0].control.set(EpCtl::ENABLE | EpCtl::CNAK);
    }
    
    /// Handle all endpoint 0 IN/OUT events; clear pending interrupt
    /// flags, swap buffers if needed, then either stall, dispatch to
    /// `handle_setup`, or dispatch to `expect_setup_packet` depending
    /// on whether the setup packet is ready.
    fn handle_endpoint0_events(&self, inter_out: bool, inter_in: bool) {
        let ep_out = &self.registers.out_endpoints[0];
        let ep_out_interrupts = ep_out.interrupt.get();
        if inter_out {
            ep_out.interrupt.set(ep_out_interrupts);
        }

        let ep_in = &self.registers.in_endpoints[0];
        let ep_in_interrupts = ep_in.interrupt.get();
        if inter_in {
            ep_in.interrupt.set(ep_in_interrupts);
        }

        // If the transfer is compelte (XferCompl), swap which EP0
        // OUT descriptor to use so stack can immediately receive again.
        if inter_out && ep_out_interrupts & (OutInterruptMask::XferComplMsk as u32) != 0 {
            self.swap_ep0_out_descriptors();
        }
        
        let transfer_type = TableCase::decode_interrupt(ep_out_interrupts);
        usb_debug!("USB: handle endpoint 0, transfer type: {:?}\n", transfer_type);
        let flags = self.ep0_out_descriptors
            .map(|descs| descs[self.last_out_idx.get()].flags)
            .unwrap();
        let setup_ready = flags & DescFlag::SETUP_READY == DescFlag::SETUP_READY;

        match self.state.get() {
            USBState::WaitingForSetupPacket => {
                usb_debug!("USB: waiting for setup in\n");
                if transfer_type == TableCase::A || transfer_type == TableCase::C {
                    if setup_ready {
                        self.handle_setup(transfer_type);
                    } else {
                        
                        usb_debug!("Unhandled USB event out:{:#x} in:{:#x} ",
                                   ep_out_interrupts,
                                   ep_in_interrupts);
                        usb_debug!("flags: \n"); 
                        if (flags & DescFlag::LAST) == DescFlag::LAST                {usb_debug!(" +LAST\n");}
                        if (flags & DescFlag::SHORT) == DescFlag::SHORT              {usb_debug!(" +SHORT\n");}
                        if (flags & DescFlag::IOC) == DescFlag::IOC                  {usb_debug!(" +IOC\n");}
                        if (flags & DescFlag::SETUP_READY) == DescFlag::SETUP_READY  {usb_debug!(" +SETUP_READY\n");}
                        if (flags & DescFlag::HOST_BUSY) == DescFlag::HOST_READY     {usb_debug!(" +HOST_READY\n");}
                        if (flags & DescFlag::HOST_BUSY) == DescFlag::DMA_BUSY       {usb_debug!(" +DMA_BUSY\n");}
                        if (flags & DescFlag::HOST_BUSY) == DescFlag::DMA_DONE       {usb_debug!(" +DMA_DONE\n");}
                        if (flags & DescFlag::HOST_BUSY) == DescFlag::HOST_BUSY      {usb_debug!(" +HOST_BUSY\n");}
                        panic!("Waiting for set up packet but non-setup packet received.");
                    }
                } else if transfer_type == TableCase::B {
                    // Only happens when we're stalling, so just keep waiting
                    // for a SETUP
                    self.stall_both_fifos();
                }
            }
            USBState::DataStageIn => {
                usb_debug!("USB: state is data stage in\n");
                if inter_in &&
                    ep_in_interrupts & (InInterruptMask::XferComplMsk as u32) != 0 {
                        self.registers.in_endpoints[0].control.set(EpCtl::ENABLE);
                    }

                if inter_out {
                    if transfer_type == TableCase::B {
                        // IN detected
                        self.registers.in_endpoints[0].control.set(EpCtl::ENABLE | EpCtl::CNAK);
                        self.registers.out_endpoints[0].control.set(EpCtl::ENABLE | EpCtl::CNAK);
                    } else if transfer_type == TableCase::A || transfer_type == TableCase::C {
                        if setup_ready {
                            self.handle_setup(transfer_type);
                        } else {
                            self.expect_setup_packet();
                        }
                    }
                }
            }
            USBState::NoDataStage => {
                if inter_in && ep_in_interrupts & (AllEndpointInterruptMask::IN0 as u32) != 0 {
                    self.registers.in_endpoints[0].control.set(EpCtl::ENABLE);
                }

                if inter_out {
                    if transfer_type == TableCase::B {
                        // IN detected
                        self.registers.in_endpoints[0].control.set(EpCtl::ENABLE | EpCtl::CNAK);
                        self.registers.out_endpoints[0].control.set(EpCtl::ENABLE | EpCtl::CNAK);
                    } else if transfer_type == TableCase::A || transfer_type == TableCase::C {
                        if setup_ready {
                            self.handle_setup(transfer_type);
                        } else {
                            self.expect_setup_packet();
                        }
                    } else {
                        self.expect_setup_packet();
                    }
                }
            }
        }
    }

    /// Handle a SETUP packet to endpoint 0 OUT, dispatching to a
    /// helper function depending on what kind of a request it is;
    /// currently supports Standard requests to Device and Interface,
    /// or Class requests to Interface.
    ///
    /// `transfer_type` is the `TableCase` found by inspecting
    /// endpoint-0's interrupt register. Currently only Standard
    /// requests to Devices are supported: requests to an Interface
    /// will panic. Based on the direction of the request and data
    /// size, this function calls one of handle_setup_device_to_host,
    /// handle_setup_host_to_device (not supported), or
    /// handle_setup_no_data_phase.
    fn handle_setup(&self, transfer_type: TableCase) {
        // Assuming `ep0_out_buffers` was properly set in `init`, this will
        // always succeed.
        usb_debug!("Handle setup, case {:?}\n", transfer_type);
        self.ep0_out_buffers.get().map(|bufs| {
            let request = SetupRequest::new(&bufs[self.last_out_idx.get()]);
            usb_debug!("  - type={:?} recip={:?} dir={:?} request={:?}\n", request.req_type(), request.recipient(), request.data_direction(), request.request());
            
            if request.req_type() == SetupRequestClass::Standard {
                if request.recipient() == SetupRecipient::Device {
                    usb_debug!("Standard request on device.\n");
                    if request.data_direction() == SetupDirection::DeviceToHost {
                        self.handle_standard_device_to_host(transfer_type, &request);
                    } else if request.w_length > 0 { // Data requested
                        self.handle_standard_host_to_device(transfer_type, &request);
                    } else { // No data requested
                        self.handle_standard_no_data_phase(transfer_type, &request);
                    }
                } else if request.recipient() == SetupRecipient::Interface {
                    usb_debug!("Standard request on interface.\n");
                    if request.data_direction() == SetupDirection::DeviceToHost {
                        self.handle_standard_interface_to_host(transfer_type, &request);
                    } else {
                        self.handle_standard_host_to_interface(transfer_type, &request);
                    }
                }
            } else if request.req_type() == SetupRequestClass::Class && request.recipient() == SetupRecipient::Interface {
                if request.data_direction() == SetupDirection::DeviceToHost {
                    self.handle_class_interface_to_host(transfer_type, &request);
                } else {
                    self.handle_class_host_to_interface(transfer_type, &request);
                }
            } else {
                usb_debug!("  - unknown case.\n");
            }
        });
    }

    fn handle_standard_host_to_device(&self, _transfer_type: TableCase, _request: &SetupRequest) {
        // TODO(alevy): don't support any of these yet...
        unimplemented!();
    }


    fn handle_standard_device_to_host(&self, transfer_type: TableCase, request: &SetupRequest) {
        use self::types::SetupRequestType::*;
        use self::serialize::Serialize;
        match request.request() {
            GetDescriptor => {
                let descriptor_type: u32 = (request.w_value >> 8) as u32;
                match descriptor_type {
                    GET_DESCRIPTOR_DEVICE => {
                        let mut len = self.ep0_in_buffers.map(|buf| {
                            self.generate_device_descriptor().serialize(buf)
                        }).unwrap_or(0);
                        
                        len = ::core::cmp::min(len, request.w_length as usize);
                        self.ep0_in_descriptors.map(|descs| {
                            descs[0].flags = (DescFlag::HOST_READY |
                                              DescFlag::LAST |
                                              DescFlag::SHORT |
                                              DescFlag::IOC).bytes(len as u16);
                        });
                        
                        usb_debug!("Trying to send device descriptor.\n");
                        self.expect_data_phase_in(transfer_type);
                    },
                    GET_DESCRIPTOR_CONFIGURATION => {
                        let mut len = 0;
                        self.ep0_in_buffers.map(|buf| {
                            self.configuration_descriptor.map(|desc| {
                                len = self.get_configuration_total_length();
                                for i in 0..16 {
                                    buf[i] = desc[4 * i + 0] as u32 |
                                             (desc[4 * i + 1] as u32) << 8 |
                                             (desc[4 * i + 2] as u32) << 16 |
                                             (desc[4 * i + 3] as u32) << 24; 
                                }
                            });
                        });
                        usb_debug!("USB: Trying to send configuration descriptor, len {}\n  ", len);
                        len = ::core::cmp::min(len, request.w_length);
                        self.ep0_in_descriptors.map(|descs| {
                            descs[0].flags = (DescFlag::HOST_READY |
                                              DescFlag::LAST |
                                              DescFlag::SHORT |
                                              DescFlag::IOC).bytes(len as u16);
                        });
                        self.expect_data_phase_in(transfer_type);
                    },
                    GET_DESCRIPTOR_INTERFACE => {
                        let i = InterfaceDescriptor::new(STRING_INTERFACE2, 0, 0x03, 0, 0);
                        let mut len = 0;
                        self.ep0_in_buffers.map(|buf| {
                            len = i.into_u32_buf(buf);
                        });
                        len = ::core::cmp::min(len, request.w_length as usize);
                        self.ep0_in_descriptors.map(|descs| {
                            descs[0].flags = (DescFlag::HOST_READY |
                                              DescFlag::LAST |
                                              DescFlag::SHORT |
                                              DescFlag::IOC).bytes(len as u16);
                        });
                        self.expect_data_phase_in(transfer_type);
                    },
                    GET_DESCRIPTOR_DEVICE_QUALIFIER => {
                        usb_debug!("Trying to send device qualifier: stall both fifos.\n");
                        self.stall_both_fifos();
                    }
                    GET_DESCRIPTOR_STRING => {
                        let index = (request.w_value & 0xff) as usize;
                        self.strings.map(|strs| {
                            let str = &strs[index];
                            let mut len = 0;
                            self.ep0_in_buffers.map(|buf| {
                                len = str.into_u32_buf(buf);
                            });
                            len = ::core::cmp::min(len, request.w_length as usize);
                            self.ep0_in_descriptors.map(|descs| {
                                descs[0].flags = (DescFlag::HOST_READY |
                                              DescFlag::LAST |
                                                  DescFlag::SHORT |
                                                  DescFlag::IOC).bytes(len as u16);
                            });
                            self.expect_data_phase_in(transfer_type);
                            
                            usb_debug!("USB: requesting string descriptor {}, len: {}: {:?}", index, len, str);
                        });
                    }
                    _ => {
                        // The specification says that a not-understood request should send an
                        // error response. Cr52 just stalls, this seems to work. -pal
                        self.stall_both_fifos();
                        usb_debug!("USB: unhandled setup descriptor type: {}", descriptor_type);
                    }
                }
            }
            GetConfiguration => {
                let mut len = self.ep0_in_buffers
                    .map(|buf| self.configuration_current_value.get().serialize(buf))
                    .unwrap_or(0);

                len = ::core::cmp::min(len, request.w_length as usize);
                self.ep0_in_descriptors.map(|descs| {
                    descs[0].flags = (DescFlag::HOST_READY | DescFlag::LAST |
                                      DescFlag::SHORT | DescFlag::IOC)
                        .bytes(len as u16);
                });
                self.expect_data_phase_in(transfer_type);
            }
            GetStatus => {
                self.ep0_in_buffers.map(|buf| {
                    buf[0] = 0x0;
                });
                self.ep0_in_descriptors.map(|descs| {
                    descs[0].flags = (DescFlag::HOST_READY | DescFlag::LAST |
                                      DescFlag::SHORT | DescFlag::IOC)
                        .bytes(2);
                });
                self.expect_status_phase_in(transfer_type);
            }
            _ => {
                panic!("USB: unhandled device-to-host setup request code: {}", request.b_request as u8);
            }
        }
    }



    /// Responds to a SETUP message destined to an interface. Currently
    /// only handles GetDescriptor requests for Report descriptors, otherwise
    /// panics.
    fn handle_standard_interface_to_host(&self, transfer_type: TableCase, request: &SetupRequest) {
        usb_debug!("Handle setup interface, device to host.\n");
        let request_type = request.request();
        match request_type {
            SetupRequestType::GetDescriptor => {
                let value      = request.value();
                let descriptor = Descriptor::from_u8((value >> 8) as u8);
                let _index      = (value & 0xff) as u8;
                let len        = request.length() as usize;
                usb_debug!("  - Descriptor: {:?}, index: {}, length: {}\n", descriptor, _index, len);
                match descriptor {
                    Descriptor::Report => {
                        if U2F_REPORT_DESCRIPTOR.len() != len {
                            panic!("Requested report of length {} but length is {}", request.length(), U2F_REPORT_DESCRIPTOR.len());
                        }
                        
                        self.ep0_in_buffers.map(|buf| {
                            for i in 0..len {
                                buf[i / 4] = (U2F_REPORT_DESCRIPTOR[i] as u32) << ((3 - (i % 4))  * 8);
                            }
                            self.ep0_in_descriptors.map(|descs| {
                                descs[0].flags = (DescFlag::HOST_READY |
                                                  DescFlag::LAST |
                                                  DescFlag::SHORT |
                                                  DescFlag::IOC).bytes(len as u16);
                            });
                            self.expect_data_phase_in(transfer_type);
                        });
                    },
                    _ => panic!("Interface device to host, unhandled request")
                }
            },
            _ => panic!("Interface device to host, unhandled request: {:?}", request_type)
        }
    }

    /// Handles a setup message to an interface, host-to-device
    /// communication.  Currently not supported: panics.
    fn handle_standard_host_to_interface(&self, _transfer_type: TableCase, _request: &SetupRequest) {
        panic!("Unhandled setup: interface, host to device!");
    }

    /// Handles a setup message to a class, device-to-host
    /// communication.  Currently not supported: panics.
    fn handle_class_interface_to_host(&self, _transfer_type: TableCase, _request: &SetupRequest) {
        panic!("Unhandled setup: class, device to host.!");
    }
    
    /// Handles a setup message to a class, host-to-device
    /// communication.  Currently supports only SetIdle commands,
    /// otherwise panics.
    fn handle_class_host_to_interface(&self, _transfer_type: TableCase, request: &SetupRequest) {
        use self::types::SetupClassRequestType;
        usb_debug!("Handle setup class, host to device.\n");
        match request.class_request() {
            SetupClassRequestType::SetIdle => {
                let val = request.value();
                let _interval: u8 = (val & 0xff) as u8;
                let _id: u8 = (val >> 8) as u8;
                usb_debug!("SetIdle: {} to {}, stall fifos.", _id, _interval);
                self.stall_both_fifos();
            },
            _ => {
                panic!("Unknown handle setup case: {:?}.\n", request.class_request());
            }
        }
    }

    fn handle_standard_no_data_phase(&self, transfer_type: TableCase, request: &SetupRequest) {
        use self::types::SetupRequestType::*;
        usb_debug!(" - setup (no data): {:?}\n", request.request());
        match request.request() {
            GetStatus => {
                panic!("USB: GET_STATUS no data setup packet.");
            }
            SetAddress => {
                usb_debug!("Setting address: {:#x}.\n", request.w_value & 0x7f);
                // Even though USB wants the address to be set after the
                // IN packet handshake, the hardware knows to wait, so
                // we should just set it now.
                let mut dcfg = self.registers.device_config.get();
                dcfg &= !(0x7f << 4); // Strip address from config
                dcfg |= ((request.w_value & 0x7f) as u32) << 4; // Put in addr
                self.registers
                    .device_config
                    .set(dcfg);
                self.expect_status_phase_in(transfer_type);
            }
            SetConfiguration => {
                usb_debug!("SetConfiguration: {:?} Type {:?} transfer\n", request.w_value, transfer_type);
                self.configuration_current_value.set(request.w_value as u8);
                self.expect_status_phase_in(transfer_type);
            }
            _ => {
                panic!("USB: unhandled no data setup packet {}", request.b_request as u8);
            }
        }
    }


    /// Call to send data to the host; assumes that the data has already
    /// been put in the IN0 descriptors.
    fn expect_data_phase_in(&self, transfer_type: TableCase) {
        self.state.set(USBState::DataStageIn);
        usb_debug!("USB: expect_data_phase_in, case: {:?}\n", transfer_type);
        self.ep0_in_descriptors.map(|descs| {
            // 2. Flush fifos
            self.flush_tx_fifo(0);

            // 3. Set EP0 in DMA
            self.registers.in_endpoints[0].dma_address.set(&descs[0]);
            usb_debug!("USB: expect_data_phase_in: endpoint 0 descriptor: flags={:08x} addr={:08x} \n", descs[0].flags.0, descs[0].addr);

            // If we clear the NAK (write CNAK) then this responds to
            // a non-setup packet, leading to failure as the code
            // needs to first respond to a setup packet.
            if transfer_type == TableCase::C {
                self.registers.in_endpoints[0].control.set(EpCtl::ENABLE | EpCtl::CNAK);
            } else {
                self.registers.in_endpoints[0].control.set(EpCtl::ENABLE);
            }

            self.ep0_out_descriptors.map(|descs| {
                descs[self.next_out_idx.get()].flags =
                    (DescFlag::HOST_READY | DescFlag::LAST | DescFlag::IOC).bytes(64);
            });

            // If we clear the NAK (write CNAK) then this responds to
            // a non-setup packet, leading to failure as the code
            // needs to first respond to a setup packet.
            if transfer_type == TableCase::C {
                self.registers.out_endpoints[0].control.set(EpCtl::ENABLE | EpCtl::CNAK);
            } else {
                self.registers.out_endpoints[0].control.set(EpCtl::ENABLE);
            }
            usb_debug!("Registering for IN0 and OUT0 interrupts.\n");
            self.registers
                .device_all_ep_interrupt_mask
                .set(self.registers.device_all_ep_interrupt_mask.get() |
                     AllEndpointInterruptMask::IN0 as u32 |
                     AllEndpointInterruptMask::OUT0 as u32);
        });
    }

    /// Setup endpoint 0 for a status phase with no data phase.
    fn expect_status_phase_in(&self, transfer_type: TableCase) {
        self.state.set(USBState::NoDataStage);
        usb_debug!("USB: expect_status_phase_in, case: {:?}\n", transfer_type);

        self.ep0_in_descriptors.map(|descs| {
            // 1. Expect a zero-length in for the status phase
            // IOC, Last, Length 0, SP
            self.ep0_in_buffers.map(|buf| {
                // Address doesn't matter since length is zero
                descs[0].addr = buf.as_ptr() as usize;
            });
            descs[0].flags =
                (DescFlag::HOST_READY | DescFlag::LAST | DescFlag::SHORT | DescFlag::IOC).bytes(0);

            // 2. Flush fifos
            self.flush_tx_fifo(0);

            // 3. Set EP0 in DMA
            self.registers.in_endpoints[0].dma_address.set(&descs[0]);

            if transfer_type == TableCase::C {
                self.registers.in_endpoints[0].control.set(EpCtl::ENABLE | EpCtl::CNAK);
            } else {
                self.registers.in_endpoints[0].control.set(EpCtl::ENABLE);
            }


            self.ep0_out_descriptors.map(|descs| {
                descs[self.next_out_idx.get()].flags =
                    (DescFlag::HOST_READY | DescFlag::LAST | DescFlag::IOC).bytes(64);
            });

            if transfer_type == TableCase::C {
                self.registers.out_endpoints[0].control.set(EpCtl::ENABLE | EpCtl::CNAK);
            } else {
                self.registers.out_endpoints[0].control.set(EpCtl::ENABLE);
            }

            self.registers
                .device_all_ep_interrupt_mask
                .set(self.registers.device_all_ep_interrupt_mask.get() |
                     AllEndpointInterruptMask::IN0 as u32 |
                     AllEndpointInterruptMask::OUT0 as u32);
        });
    }

    /// Flush endpoint 0's RX FIFO
    ///
    /// # Safety
    ///
    /// Only call this when  transaction is not underway and data from this FIFO
    /// is not being copied.
    fn flush_rx_fifo(&self) {
        self.registers.reset.set(Reset::TxFFlsh as u32); // TxFFlsh

        // Wait for TxFFlsh to clear
        while self.registers.reset.get() & (Reset::TxFFlsh as u32) != 0 {}
    }

    /// Flush endpoint 0's TX FIFO
    ///
    /// `fifo_num` is 0x0-0xF for a particular fifo, or 0x10 for all fifos
    ///
    /// # Safety
    ///
    /// Only call this when  transaction is not underway and data from this FIFO
    /// is not being copied.
    fn flush_tx_fifo(&self, fifo_num: u8) {
        let reset_val = (Reset::TxFFlsh as u32) |
        (match fifo_num {
            0  => Reset::FlushFifo0,
            1  => Reset::FlushFifo1,
            2  => Reset::FlushFifo2,
            3  => Reset::FlushFifo3,
            4  => Reset::FlushFifo4,
            5  => Reset::FlushFifo5,
            6  => Reset::FlushFifo6,
            7  => Reset::FlushFifo7,
            8  => Reset::FlushFifo8,
            9  => Reset::FlushFifo9,
            10 => Reset::FlushFifo10,
            11 => Reset::FlushFifo11,
            12 => Reset::FlushFifo12,
            13 => Reset::FlushFifo13,
            14 => Reset::FlushFifo14,
            15 => Reset::FlushFifo15,
            16 => Reset::FlushFifoAll,
            _  => Reset::FlushFifoAll, // Should Panic, or make param typed
        } as u32);
        self.registers.reset.set(reset_val);

        // Wait for TxFFlsh to clear
        while self.registers.reset.get() & (Reset::TxFFlsh as u32) != 0 {}
    }

    /// Initialize hardware data fifos
    // The constants matter for correct operation and are dependent on settings
    // in the coreConsultant. If the value is too large, the transmit_fifo_size
    // register will end up being 0, which is too small to transfer anything.
    //
    // In our case, I'm not sure what the maximum size is, but `TX_FIFO_SIZE` of
    // 32 work and 512 is too large.
    fn setup_data_fifos(&self) {
        // 3. Set up data FIFO RAM
        self.registers.receive_fifo_size.set(RX_FIFO_SIZE as u32 & 0xffff);
        self.registers
            .transmit_fifo_size
            .set(((TX_FIFO_SIZE as u32) << 16) | ((RX_FIFO_SIZE as u32) & 0xffff));
        for (i, d) in self.registers.device_in_ep_tx_fifo_size.iter().enumerate() {
            let i = i as u16;
            d.set(((TX_FIFO_SIZE as u32) << 16) | (RX_FIFO_SIZE + i * TX_FIFO_SIZE) as u32);
        }

        self.flush_tx_fifo(0x10);
        self.flush_rx_fifo();

    }


    fn generate_full_configuration_descriptor(&self) {
        self.configuration_descriptor.map(|desc| {
            let attributes_u2f_in = EndpointAttributes {
                transfer: EndpointTransferType::Interrupt,
                synchronization: EndpointSynchronizationType::None,
                usage: EndpointUsageType::Data,
            };
            let attributes_u2f_out = EndpointAttributes {
                transfer: EndpointTransferType::Interrupt,
                synchronization: EndpointSynchronizationType::None,
                usage: EndpointUsageType::Data,
            };

            let attributes_shell_in = EndpointAttributes {
                transfer: EndpointTransferType::Bulk,
                synchronization: EndpointSynchronizationType::None,
                usage: EndpointUsageType::Data,
            };
            let attributes_shell_out = EndpointAttributes {
                transfer: EndpointTransferType::Bulk,
                synchronization: EndpointSynchronizationType::None,
                usage: EndpointUsageType::Data,
            };
            
            let mut config = ConfigurationDescriptor::new(2, STRING_PLATFORM, 50);
            let u2f = InterfaceDescriptor::new(STRING_INTERFACE2, 0, 3, 0, 0);
            let hid = HidDeviceDescriptor::new();
            let ep1out = EndpointDescriptor::new(0x01, attributes_u2f_out, 2);
            let ep1in  = EndpointDescriptor::new(0x81, attributes_u2f_in, 2);
            let shell = InterfaceDescriptor::new(STRING_INTERFACE1, 1, 0xFF, 80, 1);
            let ep2in  = EndpointDescriptor::new(0x82, attributes_shell_in, 10);
            let ep2out = EndpointDescriptor::new(0x02, attributes_shell_out, 0);
            
            let mut size: usize = config.length();
            size += u2f.into_u8_buf(&mut desc[size..size + u2f.length()]);
            size += hid.into_u8_buf(&mut desc[size..size + hid.length()]);
            size += ep1out.into_u8_buf(&mut desc[size..size + ep1out.length()]);
            size += ep1in.into_u8_buf(&mut desc[size..size + ep1in.length()]);
            size += shell.into_u8_buf(&mut desc[size..size + shell.length()]);
            size += ep2in.into_u8_buf(&mut desc[size..size + ep2in.length()]);
            size += ep2out.into_u8_buf(&mut desc[size..size + ep2out.length()]);
            
            config.set_total_length(size as u16);
            config.into_u8_buf(&mut desc[0..config.length()]);
            self.set_configuration_total_length(size as u16);
        });
    }

    pub fn set_configuration_total_length(&self, length: u16) {
        self.configuration_total_length.set(length);
    }

    pub fn get_configuration_total_length(&self) -> u16 {
        self.configuration_total_length.get()
    }
    
    /// Stalls both the IN and OUT endpoints for endpoint 0.
    //
    // A STALL condition indicates that an endpoint is unable to
    // transmit or receive data.  STALLing when waiting for a SETUP
    // message forces the host to send a new SETUP. This can be used to
    // indicate the request wasn't understood or needs to be resent.
    fn stall_both_fifos(&self) {
        usb_debug!("USB: WaitingForSetupPacket in stall_both_fifos.\n");
        self.state.set(USBState::WaitingForSetupPacket);
        self.ep0_out_descriptors.map(|descs| {
            descs[self.next_out_idx.get()].flags = (DescFlag::LAST | DescFlag::IOC).bytes(64);
        });

        // Enable OUT and disable IN interrupts
        let mut interrupts = self.registers.device_all_ep_interrupt_mask.get();
        interrupts |= AllEndpointInterruptMask::OUT0 as u32;
        interrupts &= !(AllEndpointInterruptMask::IN0 as u32);
        self.registers.device_all_ep_interrupt_mask.set(interrupts);

        self.registers.out_endpoints[0].control.set(EpCtl::ENABLE | EpCtl::STALL);
        self.flush_tx_fifo(0);
        self.registers.in_endpoints[0].control.set(EpCtl::ENABLE | EpCtl::STALL);
    }

    // Helper function which swaps which EP0 out descriptor is set up
    // to receive so software can receive a new packet while
    // processing the current one.
    fn swap_ep0_out_descriptors(&self) {
        self.ep0_out_descriptors.map(|descs| {
            let mut noi = self.next_out_idx.get();
            self.last_out_idx.set(noi);
            noi = (noi + 1) % descs.len();
            self.next_out_idx.set(noi);
            self.registers.out_endpoints[0].dma_address.set(&descs[noi]);
        });
    }
    
    fn generate_device_descriptor(&self) -> DeviceDescriptor {
        DeviceDescriptor {
            b_length: 18,
            b_descriptor_type: 1,
            bcd_usb: 0x0200,
            b_device_class: self.device_class.get(),
            b_device_sub_class: 0x00,
            b_device_protocol: 0x00,
            b_max_packet_size0: MAX_PACKET_SIZE as u8,
            id_vendor: self.vendor_id.get(),
            id_product: self.product_id.get(),
            bcd_device: 0x0100,
            i_manufacturer: STRING_VENDOR,
            i_product: STRING_BOARD,
            i_serial_number: STRING_LANG,
            b_num_configurations: 1,
        }
    }
}

/// Which physical connection to use
pub enum PHY {
    A,
    B,
}

/// Combinations of OUT endpoint interrupts for control transfers denote
/// different transfer cases.
///
/// TableCase encodes the cases from Table 10.7 in the OTG Programming
/// Guide (pages 279-230).
#[derive(Copy,Clone,PartialEq,Eq,Debug)]
pub enum TableCase {
    /// Case A
    ///
    /// * StsPhseRcvd: 0
    /// * SetUp: 0
    /// * XferCompl: 1
    A,   // OUT descriptor updated; check the SR bit to see if Setup or OUT
    /// Case B
    ///
    /// * StsPhseRcvd: 0
    /// * SetUp: 1
    /// * XferCompl: 0
    B,   // Setup Phase Done for previously decoded Setup packet
    /// Case C
    ///
    /// * StsPhseRcvd: 0
    /// * SetUp: 1
    /// * XferCompl: 1
    C,   // OUT descriptor updated for a Setup packet, Setup complete
    /// Case D
    ///
    /// * StsPhseRcvd: 1
    /// * SetUp: 0
    /// * XferCompl: 0
    D,   // Status phase of Control OUT transfer
    /// Case E
    ///
    /// * StsPhseRcvd: 1
    /// * SetUp: 0
    /// * XferCompl: 1
    E,   // OUT descriptor updated; check SR bit to see if Setup or Out.
         // Plus, host is now in Control Write Status phase
}

impl TableCase {
    /// Decodes a value from the OUT endpoint interrupt register.
    ///
    /// Only properly decodes values with the combinations shown in the
    /// programming guide.
    pub fn decode_interrupt(device_out_int: u32) -> TableCase {
        if device_out_int & (OutInterruptMask::XferComplMsk as u32) != 0 {
            if device_out_int & (OutInterruptMask::SetUPMsk as u32) != 0 {
                TableCase::C
            } else if device_out_int & (OutInterruptMask::StsPhseRcvdMsk as u32) != 0 {
                TableCase::E
            } else {
                TableCase::A
            }
        } else {
            if device_out_int & (OutInterruptMask::SetUPMsk as u32) != 0 {
                TableCase::B
            } else {
                TableCase::D
            }
        }
    }
}

fn print_usb_interrupt_status(status: u32) {
    usb_debug!("USB interrupt, status: {:08x}\n", status);
    if (status & Interrupt::HostMode as u32) != 0           {usb_debug!("  +Host mode\n");}
    if (status & Interrupt::Mismatch as u32) != 0           {usb_debug!("  +Mismatch\n");}
    if (status & Interrupt::OTG as u32) != 0                {usb_debug!("  +OTG\n");}
    if (status & Interrupt::SOF as u32) != 0                {usb_debug!("  +SOF\n");}
    if (status & Interrupt::RxFIFO as u32) != 0             {usb_debug!("  +RxFIFO\n");}
    if (status & Interrupt::GlobalInNak as u32) != 0        {usb_debug!("  +GlobalInNak\n");}
    if (status & Interrupt::OutNak as u32) != 0             {usb_debug!("  +OutNak\n");}
    if (status & Interrupt::EarlySuspend as u32) != 0       {usb_debug!("  +EarlySuspend\n");}
    if (status & Interrupt::Suspend as u32) != 0            {usb_debug!("  +Suspend\n");}
    if (status & Interrupt::Reset as u32) != 0              {usb_debug!("  +USB reset\n");}
    if (status & Interrupt::EnumDone as u32) != 0           {usb_debug!("  +Speed enum done\n");}
    if (status & Interrupt::OutISOCDrop as u32) != 0        {usb_debug!("  +Out ISOC drop\n");}
    if (status & Interrupt::EOPF as u32) != 0               {usb_debug!("  +EOPF\n");}
    if (status & Interrupt::EndpointMismatch as u32) != 0   {usb_debug!("  +Endpoint mismatch\n");}
    if (status & Interrupt::InEndpoints as u32) != 0        {usb_debug!("  +IN endpoints\n");}
    if (status & Interrupt::OutEndpoints as u32) != 0       {usb_debug!("  +OUT endpoints\n");}
    if (status & Interrupt::InISOCIncomplete as u32) != 0   {usb_debug!("  +IN ISOC incomplete\n");}
    if (status & Interrupt::IncompletePeriodic as u32) != 0 {usb_debug!("  +Incomp periodic\n");}
    if (status & Interrupt::FetchSuspend as u32) != 0       {usb_debug!("  +Fetch suspend\n");}
    if (status & Interrupt::ResetDetected as u32) != 0      {usb_debug!("  +Reset detected\n");}
    if (status & Interrupt::ConnectIDChange as u32) != 0    {usb_debug!("  +Connect ID change\n");}
    if (status & Interrupt::SessionRequest as u32) != 0     {usb_debug!("  +Session request\n");}
    if (status & Interrupt::ResumeWakeup as u32) != 0       {usb_debug!("  +Resume/wakeup\n");}
}
