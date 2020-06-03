//! Communications Class Device for USB
//!
//! This capsule allows Tock to support a serial port over USB.

use core::cell::Cell;
use core::cmp;

use super::descriptors;
use super::descriptors::Buffer64;
use super::descriptors::CdcInterfaceDescriptor;
use super::descriptors::EndpointAddress;
use super::descriptors::EndpointDescriptor;
use super::descriptors::InterfaceDescriptor;
use super::descriptors::TransferDirection;
use super::usbc_client_ctrl::ClientCtrl;

use kernel::common::cells::OptionalCell;
use kernel::common::cells::TakeCell;
use kernel::common::cells::VolatileCell;
use kernel::hil;
use kernel::hil::uart;
use kernel::hil::usb::TransferType;
use kernel::ReturnCode;

const VENDOR_ID: u16 = 0x6668;
const PRODUCT_ID: u16 = 0xabce;

/// Identifying number for the endpoint when transferring data from us to the
/// host.
const ENDPOINT_IN_NUM: usize = 2;
/// Identifying number for the endpoint when transferring data from the host to
/// us.
const ENDPOINT_OUT_NUM: usize = 3;

static LANGUAGES: &'static [u16; 1] = &[
    0x0409, // English (United States)
];

static STRINGS: &'static [&'static str] = &[
    "TockOS",         // Manufacturer
    "The Zorpinator", // Product
    "123456",         // Serial number
];

/// Platform-specific packet length for the `SAM4L` USB hardware.
pub const MAX_CTRL_PACKET_SIZE_SAM4L: u8 = 8;
/// Platform-specific packet length for the `nRF52` USB hardware.
pub const MAX_CTRL_PACKET_SIZE_NRF52840: u8 = 64;

const N_ENDPOINTS: usize = 3;

/// Implementation of the Abstract Control Model (ACM) for the Communications
/// Class Device (CDC) over USB.
pub struct CdcAcm<'a, U: 'a> {
    /// Helper USB client library for handling many USB operations.
    client_ctrl: ClientCtrl<'a, 'static, U>,

    /// 64 byte buffers for each endpoint.
    buffers: [Buffer64; N_ENDPOINTS],

    /// A holder reference for the TX buffer we are transmitting from.
    tx_buffer: TakeCell<'static, [u8]>,
    /// The number of bytes the client has asked us to send. We track this so we
    /// can pass it back to the client when the transmission has finished.
    tx_len: Cell<usize>,
    /// How many more bytes we need to transmit. This is used in our TX state
    /// machine.
    tx_remaining: Cell<usize>,
    /// Where in the `tx_buffer` we need to start sending from when we continue.
    tx_offset: Cell<usize>,
    /// The TX client to use when transmissions finish.
    tx_client: OptionalCell<&'a dyn uart::TransmitClient>,

    /// A holder for the buffer to receive bytes into. We use this as a flag as
    /// well, if we have a buffer then we are actively doing a receive.
    rx_buffer: TakeCell<'static, [u8]>,
    /// How many bytes the client wants us to receive.
    rx_len: Cell<usize>,
    /// How many bytes we have received so far.
    rx_offset: Cell<usize>,
    /// The RX client to use when RX data is received.
    rx_client: OptionalCell<&'a dyn uart::ReceiveClient>,
}

impl<'a, U: hil::usb::UsbController<'a>> CdcAcm<'a, U> {
    pub fn new(controller: &'a U, max_ctrl_packet_size: u8) -> Self {
        let interfaces: &mut [InterfaceDescriptor] = &mut [
            InterfaceDescriptor {
                interface_number: 0,
                interface_class: 0x02,    // CDC communication
                interface_subclass: 0x02, // abstract control model (ACM)
                interface_protocol: 0x01, // V.25ter (AT commands)
                ..InterfaceDescriptor::default()
            },
            InterfaceDescriptor {
                interface_number: 1,
                interface_class: 0x0a,    // CDC data
                interface_subclass: 0x00, // none
                interface_protocol: 0x00, // none
                ..InterfaceDescriptor::default()
            },
        ];

        let cdc_descriptors: &mut [CdcInterfaceDescriptor] = &mut [
            CdcInterfaceDescriptor {
                subtype: descriptors::CdcInterfaceDescriptorSubType::Header,
                field1: 0x10, // CDC
                field2: 0x11, // CDC
            },
            CdcInterfaceDescriptor {
                subtype: descriptors::CdcInterfaceDescriptorSubType::CallManagement,
                field1: 0x00, // Capabilities
                field2: 0x01, // Data interface 1
            },
            CdcInterfaceDescriptor {
                subtype: descriptors::CdcInterfaceDescriptorSubType::AbstractControlManagement,
                field1: 0x06, // Capabilities
                field2: 0x00, // unused
            },
            CdcInterfaceDescriptor {
                subtype: descriptors::CdcInterfaceDescriptorSubType::Union,
                field1: 0x00, // Interface 0
                field2: 0x01, // Interface 1
            },
        ];

        let endpoints: &[&[EndpointDescriptor]] = &[
            &[EndpointDescriptor {
                endpoint_address: EndpointAddress::new_const(4, TransferDirection::DeviceToHost),
                transfer_type: TransferType::Interrupt,
                max_packet_size: 8,
                interval: 100,
            }],
            &[
                EndpointDescriptor {
                    endpoint_address: EndpointAddress::new_const(
                        2,
                        TransferDirection::DeviceToHost,
                    ),
                    transfer_type: TransferType::Bulk,
                    max_packet_size: 64,
                    interval: 100,
                },
                EndpointDescriptor {
                    endpoint_address: EndpointAddress::new_const(
                        3,
                        TransferDirection::HostToDevice,
                    ),
                    transfer_type: TransferType::Bulk,
                    max_packet_size: 64,
                    interval: 100,
                },
            ],
        ];

        let (device_descriptor_buffer, other_descriptor_buffer) =
            descriptors::create_descriptor_buffers(
                descriptors::DeviceDescriptor {
                    vendor_id: VENDOR_ID,
                    product_id: PRODUCT_ID,
                    manufacturer_string: 1,
                    product_string: 2,
                    serial_number_string: 3,
                    class: 0x2, // Class: CDC
                    max_packet_size_ep0: max_ctrl_packet_size,
                    ..descriptors::DeviceDescriptor::default()
                },
                descriptors::ConfigurationDescriptor {
                    ..descriptors::ConfigurationDescriptor::default()
                },
                interfaces,
                endpoints,
                None, // No HID descriptor
                Some(cdc_descriptors),
            );

        CdcAcm {
            client_ctrl: ClientCtrl::new(
                controller,
                device_descriptor_buffer,
                other_descriptor_buffer,
                None, // No HID descriptor
                None, // No report descriptor
                LANGUAGES,
                STRINGS,
            ),
            buffers: [
                Buffer64::default(),
                Buffer64::default(),
                Buffer64::default(),
            ],
            tx_buffer: TakeCell::empty(),
            tx_len: Cell::new(0),
            tx_remaining: Cell::new(0),
            tx_offset: Cell::new(0),
            tx_client: OptionalCell::empty(),
            rx_buffer: TakeCell::empty(),
            rx_len: Cell::new(0),
            rx_offset: Cell::new(0),
            rx_client: OptionalCell::empty(),
        }
    }

    #[inline]
    fn controller(&self) -> &'a U {
        self.client_ctrl.controller()
    }

    #[inline]
    fn buffer(&'a self, i: usize) -> &'a [VolatileCell<u8>; 64] {
        &self.buffers[i - 1].buf
    }
}

impl<'a, U: hil::usb::UsbController<'a>> hil::usb::Client<'a> for CdcAcm<'a, U> {
    fn enable(&'a self) {
        // Set up the default control endpoint
        self.client_ctrl.enable();

        // Setup buffers for IN and OUT data transfer.
        self.controller()
            .endpoint_set_in_buffer(ENDPOINT_IN_NUM, self.buffer(ENDPOINT_IN_NUM));
        self.controller()
            .endpoint_in_enable(TransferType::Bulk, ENDPOINT_IN_NUM);

        self.controller()
            .endpoint_set_out_buffer(ENDPOINT_OUT_NUM, self.buffer(ENDPOINT_OUT_NUM));
        self.controller()
            .endpoint_out_enable(TransferType::Bulk, ENDPOINT_OUT_NUM);
    }

    fn attach(&'a self) {
        self.client_ctrl.attach();
    }

    fn bus_reset(&'a self) {
        // No need to handle this at this layer.
    }

    /// Handle a Control Setup transaction
    fn ctrl_setup(&'a self, endpoint: usize) -> hil::usb::CtrlSetupResult {
        self.client_ctrl.ctrl_setup(endpoint)
    }

    /// Handle a Control In transaction
    fn ctrl_in(&'a self, endpoint: usize) -> hil::usb::CtrlInResult {
        self.client_ctrl.ctrl_in(endpoint)
    }

    /// Handle a Control Out transaction
    fn ctrl_out(&'a self, endpoint: usize, packet_bytes: u32) -> hil::usb::CtrlOutResult {
        // Hack to make sure we ask to send data if we have a buffer queued. We
        // expect control out messages when the host actually connects via CDC,
        // so we use this to generate the data IN request.
        if self.tx_buffer.is_some() {
            self.controller().endpoint_resume_in(ENDPOINT_IN_NUM);
        }

        self.client_ctrl.ctrl_out(endpoint, packet_bytes)
    }

    fn ctrl_status(&'a self, endpoint: usize) {
        self.client_ctrl.ctrl_status(endpoint)
    }

    /// Handle the completion of a Control transfer
    fn ctrl_status_complete(&'a self, endpoint: usize) {
        self.client_ctrl.ctrl_status_complete(endpoint)
    }

    /// Handle a Bulk/Interrupt IN transaction.
    ///
    /// This is called when we can send data to the host. It should get called
    /// when we tell the controller we want to resume the IN endpoint (meaning
    /// we know we have data to send) and afterwards until we return
    /// `hil::usb::InResult::Delay` from this function. That means we can use
    /// this as a callback to mean that the transmission finished by waiting
    /// until this function is called when we don't have anything left to send.
    fn packet_in(&'a self, transfer_type: TransferType, endpoint: usize) -> hil::usb::InResult {
        match transfer_type {
            TransferType::Bulk => {
                self.tx_buffer
                    .take()
                    .map_or(hil::usb::InResult::Delay, |tx_buf| {
                        // Check if we have any bytes to send.
                        let remaining = self.tx_remaining.get();
                        if remaining > 0 {
                            // We do, so we go ahead and send those.

                            // Get packet that we have shared with the underlying
                            // USB stack to copy the tx into.
                            let packet = self.buffer(endpoint);

                            // Calculate how much more we can send.
                            let to_send = cmp::min(packet.len(), remaining);

                            // Copy from the TX buffer to the outgoing USB packet.
                            let offset = self.tx_offset.get();
                            for i in 0..to_send {
                                packet[i].set(tx_buf[offset + i]);
                            }

                            // Update our state on how much more there is to send.
                            self.tx_remaining.set(remaining - to_send);
                            self.tx_offset.set(offset + to_send);

                            // Put the TX buffer back so we can keep sending from it.
                            self.tx_buffer.replace(tx_buf);

                            // Return that we have data to send.
                            hil::usb::InResult::Packet(to_send)
                        } else {
                            // We don't have anything to send, so that means we are
                            // ok to signal the callback.

                            // Signal the callback and pass back the TX buffer.
                            self.tx_client.map(move |tx_client| {
                                tx_client.transmitted_buffer(
                                    tx_buf,
                                    self.tx_len.get(),
                                    ReturnCode::SUCCESS,
                                )
                            });

                            // Return that we have nothing else to do to the USB
                            // driver.
                            hil::usb::InResult::Delay
                        }
                    })
            }
            TransferType::Control | TransferType::Isochronous | TransferType::Interrupt => {
                // Nothing to do for CDC ACM.
                hil::usb::InResult::Delay
            }
        }
    }

    /// Handle a Bulk/Interrupt OUT transaction
    fn packet_out(
        &'a self,
        transfer_type: TransferType,
        endpoint: usize,
        packet_bytes: u32,
    ) -> hil::usb::OutResult {
        match transfer_type {
            TransferType::Bulk => {
                // Start by checking to see if we even care about this RX or
                // not.
                self.rx_buffer.take().map(|rx_buf| {
                    let rx_offset = self.rx_offset.get();

                    // How many more bytes can we store in our RX buffer?
                    let available_bytes = rx_buf.len() - rx_offset;
                    let copy_length = cmp::min(packet_bytes as usize, available_bytes);

                    // Do the copy into the RX buffer.
                    let packet = self.buffer(endpoint);
                    for i in 0..copy_length {
                        rx_buf[rx_offset + i] = packet[i].get();
                    }

                    // Keep track of how many bytes we have received so far.
                    let total_received_bytes = rx_offset + copy_length;

                    // Update how many bytes we have gotten.
                    self.rx_offset.set(total_received_bytes);

                    // Check if we have received at least as many bytes as the
                    // client asked for.
                    if total_received_bytes >= self.rx_len.get() {
                        self.rx_client.map(move |client| {
                            client.received_buffer(
                                rx_buf,
                                total_received_bytes,
                                ReturnCode::SUCCESS,
                                uart::Error::None,
                            );
                        });
                    } else {
                        // Make sure to put the RX buffer back.
                        self.rx_buffer.replace(rx_buf);
                    }
                });

                // No error cases to report to the USB.
                hil::usb::OutResult::Ok
            }
            TransferType::Control | TransferType::Isochronous | TransferType::Interrupt => {
                // Nothing to do for CDC ACM.
                hil::usb::OutResult::Ok
            }
        }
    }

    fn packet_transmitted(&'a self, _endpoint: usize) {
        // Check if more to send.
        self.tx_buffer.take().map(|tx_buf| {
            // Check if we have any bytes to send.
            let remaining = self.tx_len.get() - self.tx_offset.get();
            if remaining > 0 {
                // We do, so ask to send again.
                self.tx_buffer.replace(tx_buf);
                self.controller().endpoint_resume_in(ENDPOINT_IN_NUM);
            } else {
                // We don't have anything to send, so that means we are
                // ok to signal the callback.

                // Signal the callback and pass back the TX buffer.
                self.tx_client.map(move |tx_client| {
                    tx_client.transmitted_buffer(tx_buf, self.tx_len.get(), ReturnCode::SUCCESS)
                });
            }
        });
    }
}

impl<'a, U: hil::usb::UsbController<'a>> uart::Configure for CdcAcm<'a, U> {
    fn configure(&self, _parameters: uart::Parameters) -> ReturnCode {
        // Since this is not a real UART, we don't need to consider these
        // parameters.
        ReturnCode::SUCCESS
    }
}

impl<'a, U: hil::usb::UsbController<'a>> uart::Transmit<'a> for CdcAcm<'a, U> {
    fn set_transmit_client(&self, client: &'a dyn uart::TransmitClient) {
        self.tx_client.set(client);
    }

    fn transmit_buffer(
        &self,
        tx_buffer: &'static mut [u8],
        tx_len: usize,
    ) -> (ReturnCode, Option<&'static mut [u8]>) {
        if self.tx_buffer.is_some() {
            // We are already handling a transmission, we cannot queue another
            // request.
            (ReturnCode::EBUSY, Some(tx_buffer))
        } else if tx_len > tx_buffer.len() {
            // Can't send more bytes than will fit in the buffer.
            (ReturnCode::ESIZE, Some(tx_buffer))
        } else {
            // Ok, we can handle this transmission. Initialize all of our state
            // for our TX state machine.
            self.tx_remaining.set(tx_len);
            self.tx_len.set(tx_len);
            self.tx_offset.set(0);
            self.tx_buffer.replace(tx_buffer);

            // Then signal to the lower layer that we are ready to do a TX by
            // putting data in the IN endpoint.
            self.controller().endpoint_resume_in(ENDPOINT_IN_NUM);

            (ReturnCode::SUCCESS, None)
        }
    }

    fn transmit_abort(&self) -> ReturnCode {
        ReturnCode::FAIL
    }

    fn transmit_word(&self, _word: u32) -> ReturnCode {
        ReturnCode::FAIL
    }
}

impl<'a, U: hil::usb::UsbController<'a>> uart::Receive<'a> for CdcAcm<'a, U> {
    fn set_receive_client(&self, client: &'a dyn uart::ReceiveClient) {
        self.rx_client.set(client);
    }

    fn receive_buffer(
        &self,
        rx_buffer: &'static mut [u8],
        rx_len: usize,
    ) -> (ReturnCode, Option<&'static mut [u8]>) {
        if self.rx_buffer.is_some() {
            (ReturnCode::EBUSY, Some(rx_buffer))
        } else if rx_len > rx_buffer.len() {
            (ReturnCode::ESIZE, Some(rx_buffer))
        } else {
            self.rx_buffer.replace(rx_buffer);
            self.rx_offset.set(0);
            self.rx_len.set(rx_len);

            (ReturnCode::SUCCESS, None)
        }
    }

    fn receive_abort(&self) -> ReturnCode {
        ReturnCode::FAIL
    }

    fn receive_word(&self) -> ReturnCode {
        ReturnCode::FAIL
    }
}

impl<'a, U: hil::usb::UsbController<'a>> uart::Uart<'a> for CdcAcm<'a, U> {}
impl<'a, U: hil::usb::UsbController<'a>> uart::UartData<'a> for CdcAcm<'a, U> {}
